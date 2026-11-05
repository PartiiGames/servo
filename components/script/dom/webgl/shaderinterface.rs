/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The interface between a program's two shader stages as their sources declare it, for the
//! link-time rules WebGL has and desktop GL drivers don't check: uniform (and uniform block
//! member) precisions, invariance of the built-ins the fragment shader reads, and the GLSL ES
//! 1.00 varying packing limit (Appendix A.7). ANGLE's translator doesn't give the embedder its
//! variable lists here, so they are read from the source, conservatively: a source with macros,
//! conditionals or a global declaration this doesn't understand gets no interface, and the
//! link is then left to the driver as before.

use std::collections::{HashMap, HashSet};

/// The declarations of one shader stage that the link checks compare.
#[derive(Debug, Default)]
pub(crate) struct ShaderInterface {
    /// Uniforms outside blocks by name: their type with every precision resolved.
    uniforms: HashMap<String, String>,
    /// Uniform blocks by block name: their members with every precision resolved.
    blocks: HashMap<String, String>,
    /// Varyings (`varying`, or ESSL 3.00 `in`/`out` between the stages) by name.
    varyings: HashMap<String, Varying>,
    /// Names declared invariant (`invariant gl_Position;` and invariant declarations).
    invariant: HashSet<String>,
    /// `#pragma STDGL invariant(all)`: every output is invariant.
    invariant_all: bool,
    /// Identifiers read or written in function bodies (static use).
    used: HashSet<String>,
}

#[derive(Clone, Debug)]
struct Varying {
    type_name: String,
    array_size: u32,
}

/// Stage defaults (GLSL ES 1.00 4.5.3, 3.00 4.5.4) by base type; `precision` statements
/// override them from there on.
fn default_precisions(vertex: bool) -> HashMap<&'static str, &'static str> {
    let mut defaults = HashMap::new();
    if vertex {
        defaults.insert("float", "highp");
        defaults.insert("int", "highp");
    } else {
        defaults.insert("int", "mediump");
    }
    defaults
}

/// The precision base type of a basic type, if precision applies to it. Samplers are left out:
/// their precision isn't compared.
fn precision_base(type_name: &str) -> Option<&'static str> {
    match type_name {
        "float" | "vec2" | "vec3" | "vec4" | "mat2" | "mat3" | "mat4" | "mat2x2" | "mat2x3" |
        "mat2x4" | "mat3x2" | "mat3x3" | "mat3x4" | "mat4x2" | "mat4x3" | "mat4x4" => Some("float"),
        "int" | "ivec2" | "ivec3" | "ivec4" | "uint" | "uvec2" | "uvec3" | "uvec4" => Some("int"),
        _ => None,
    }
}

fn is_basic_type(type_name: &str) -> bool {
    precision_base(type_name).is_some() ||
        matches!(type_name, "bool" | "bvec2" | "bvec3" | "bvec4") ||
        type_name.starts_with("sampler") ||
        type_name.starts_with("isampler") ||
        type_name.starts_with("usampler")
}

fn is_precision(token: &str) -> bool {
    matches!(token, "highp" | "mediump" | "lowp")
}

fn is_identifier(token: &str) -> bool {
    token
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_') &&
        token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A global statement: tokens, with a `{ ... }` (struct or block body) as one item.
#[derive(Debug)]
enum Item {
    Token(String),
    Body(Vec<Vec<String>>),
}

impl Item {
    fn token(&self) -> Option<&str> {
        match self {
            Item::Token(token) => Some(token),
            Item::Body(_) => None,
        }
    }
}

fn tokenize(code: &str) -> Vec<String> {
    let mut tokens = vec![];
    let chars: Vec<char> = code.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_alphanumeric() || c == '_' {
            let start = i;
            // A number keeps its '.', exponent and suffix; an identifier stops at '.'.
            let number = c.is_ascii_digit();
            while i < chars.len() &&
                (chars[i].is_ascii_alphanumeric() ||
                    chars[i] == '_' ||
                    (number && chars[i] == '.'))
            {
                i += 1;
            }
            tokens.push(chars[start..i].iter().collect());
        } else {
            tokens.push(c.to_string());
            i += 1;
        }
    }
    tokens
}

impl ShaderInterface {
    /// Reads the interface of a shader from its source. None when it can't be read for sure.
    pub(crate) fn parse(source: &str, vertex: bool) -> Option<ShaderInterface> {
        let mut interface = ShaderInterface::default();
        let mut code = String::with_capacity(source.len());
        let mut rest = source;
        while !rest.is_empty() {
            if let Some(after) = rest.strip_prefix("//") {
                rest = after.split_once('\n').map_or("", |(_, after)| after);
                code.push('\n');
            } else if let Some(after) = rest.strip_prefix("/*") {
                rest = after.split_once("*/").map_or("", |(_, after)| after);
                code.push(' ');
            } else {
                let c = rest.chars().next()?;
                code.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
        let mut body = String::with_capacity(code.len());
        for line in code.lines() {
            let trimmed = line.trim_start();
            if let Some(directive) = trimmed.strip_prefix('#') {
                let words: Vec<&str> = directive
                    .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
                    .filter(|word| !word.is_empty())
                    .collect();
                match words.first().copied() {
                    Some("version" | "extension") | None => {},
                    Some("pragma") => {
                        if words.get(1) == Some(&"STDGL") &&
                            words.get(2) == Some(&"invariant") &&
                            words.get(3) == Some(&"all")
                        {
                            interface.invariant_all = true;
                        }
                    },
                    _ => return None,
                }
                continue;
            }
            body.push_str(line);
            body.push('\n');
        }

        // Split into global statements; function bodies only feed `used`.
        let tokens = tokenize(&body);
        let mut statements: Vec<Vec<Item>> = vec![];
        let mut current: Vec<Item> = vec![];
        let mut i = 0;
        while i < tokens.len() {
            let token = &tokens[i];
            match token.as_str() {
                ";" => statements.push(std::mem::take(&mut current)),
                "{" => {
                    // `f(...) {` is a function body; `struct S {` and `uniform B {` aren't.
                    let function = current.last().and_then(Item::token) == Some(")");
                    let mut depth = 1;
                    let mut members = vec![vec![]];
                    i += 1;
                    while depth > 0 {
                        let token = tokens.get(i)?;
                        match token.as_str() {
                            "{" => {
                                if !function {
                                    // A struct or block inside a struct or block.
                                    return None;
                                }
                                depth += 1;
                            },
                            "}" => depth -= 1,
                            ";" if !function => members.push(vec![]),
                            _ if function => {
                                if is_identifier(token) {
                                    interface.used.insert(token.clone());
                                }
                            },
                            _ => members.last_mut()?.push(token.clone()),
                        }
                        i += 1;
                    }
                    if function {
                        current.clear();
                    } else {
                        if !members.last()?.is_empty() {
                            return None;
                        }
                        members.pop();
                        current.push(Item::Body(members));
                    }
                    continue;
                },
                "}" => return None,
                _ => current.push(Item::Token(token.clone())),
            }
            i += 1;
        }
        if !current.is_empty() {
            return None;
        }

        let mut defaults = default_precisions(vertex);
        let mut structs: HashMap<String, String> = HashMap::new();
        for statement in statements {
            interface.read_statement(&statement, vertex, &mut defaults, &mut structs)?;
        }
        Some(interface)
    }

    fn read_statement(
        &mut self,
        statement: &[Item],
        vertex: bool,
        defaults: &mut HashMap<&'static str, &'static str>,
        structs: &mut HashMap<String, String>,
    ) -> Option<()> {
        let mut items = statement.iter().peekable();
        let Some(first) = items.peek() else {
            return Some(());
        };
        // precision <p> <type>;
        if first.token() == Some("precision") {
            let tokens: Vec<_> = statement.iter().map(Item::token).collect();
            let [_, Some(precision), Some(type_name)] = tokens[..] else {
                return None;
            };
            let precision = match precision {
                "highp" => "highp",
                "mediump" => "mediump",
                "lowp" => "lowp",
                _ => return None,
            };
            if let Some(base) = precision_base(type_name)
                .filter(|_| type_name == "float" || type_name == "int" || type_name == "uint")
            {
                defaults.insert(base, precision);
            }
            return Some(());
        }

        let mut qualifiers = vec![];
        let mut precision = None;
        loop {
            match items.peek()?.token() {
                Some("layout") => {
                    items.next();
                    if items.next()?.token()? != "(" {
                        return None;
                    }
                    while items.next()?.token()? != ")" {}
                },
                Some(
                    qualifier @ ("invariant" | "uniform" | "varying" | "attribute" | "in" | "out" |
                    "const" | "flat" | "smooth" | "centroid"),
                ) => {
                    qualifiers.push(qualifier);
                    items.next();
                },
                Some(token) if is_precision(token) => {
                    precision = Some(token);
                    items.next();
                },
                _ => break,
            }
        }
        let has = |qualifier: &str| qualifiers.contains(&qualifier);

        // `invariant name, name;` redeclares outputs (or gl_FragCoord etc.) invariant.
        if qualifiers == ["invariant"] && precision.is_none() {
            loop {
                let name = items.next()?.token()?;
                if !is_identifier(name) {
                    return None;
                }
                self.invariant.insert(name.to_owned());
                match items.next() {
                    None => return Some(()),
                    Some(item) if item.token() == Some(",") => {},
                    Some(_) => return None,
                }
            }
        }

        // Only struct definitions and declarations the stages share matter here (not function
        // prototypes, constants or attributes).
        let shared = ["uniform", "varying", "in", "out", "invariant"]
            .iter()
            .any(|qualifier| has(qualifier));
        if !shared && items.peek()?.token() != Some("struct") {
            return Some(());
        }

        let type_item = items.next()?;
        let type_token = type_item.token()?;
        // A uniform block: `uniform Name { members } [instance];`.
        if has("uniform") && !is_basic_type(type_token) && type_token != "struct" {
            if let Some(Item::Body(members)) = items.peek() {
                let members = Self::members(members, defaults, structs)?;
                self.blocks.insert(type_token.to_owned(), members);
                return Some(());
            }
        }
        let type_name = if type_token == "struct" {
            let mut name = None;
            if let Some(item) = items.peek() &&
                let Some(token) = item.token()
            {
                name = Some(token.to_owned());
                items.next();
            }
            let Some(Item::Body(members)) = items.next() else {
                return None;
            };
            let definition = format!(
                "struct {}{{{}}}",
                name.as_deref().unwrap_or(""),
                Self::members(members, defaults, structs)?
            );
            if let Some(name) = name {
                structs.insert(name, definition.clone());
            }
            definition
        } else if is_basic_type(type_token) {
            match precision_base(type_token) {
                Some(base) => format!(
                    "{} {type_token}",
                    precision
                        .or_else(|| defaults.get(base).copied())
                        .unwrap_or("?")
                ),
                None => type_token.to_owned(),
            }
        } else {
            structs.get(type_token)?.clone()
        };

        let uniform = has("uniform");
        let varying =
            has("varying") || (vertex && has("out")) || (!vertex && has("in") && !has("uniform"));
        // Declarators: name [ '[' size ']' ] [= initializer], ...
        loop {
            let Some(item) = items.next() else {
                return Some(());
            };
            let name = item.token()?;
            if !is_identifier(name) {
                return None;
            }
            let mut array_size = None;
            let mut next = items.next();
            if next.as_ref().and_then(|item| item.token()) == Some("[") {
                let size = items.next()?.token()?;
                if items.next()?.token()? != "]" {
                    return None;
                }
                array_size = Some(size.parse::<u32>().ok()?);
                next = items.next();
            }
            if uniform {
                let full_type = match array_size {
                    Some(size) => format!("{type_name}[{size}]"),
                    None => type_name.clone(),
                };
                self.uniforms.insert(name.to_owned(), full_type);
            } else if varying {
                self.varyings.insert(
                    name.to_owned(),
                    Varying {
                        type_name: type_token.to_owned(),
                        array_size: array_size.unwrap_or(1),
                    },
                );
            }
            if has("invariant") {
                self.invariant.insert(name.to_owned());
            }
            match next.and_then(Item::token) {
                None => return Some(()),
                Some(",") => {},
                Some("=") => {
                    if uniform || varying {
                        return None;
                    }
                    // A const initializer runs to the end of the statement here.
                    return Some(());
                },
                Some(_) => return None,
            }
        }
    }

    /// Struct or block members as `precision type name[size];` with every precision resolved.
    fn members(
        members: &[Vec<String>],
        defaults: &HashMap<&'static str, &'static str>,
        structs: &HashMap<String, String>,
    ) -> Option<String> {
        let mut out = String::new();
        for member in members {
            let mut tokens = member.iter().map(String::as_str).peekable();
            let mut precision = None;
            while let Some(&token) = tokens.peek() {
                if is_precision(token) {
                    precision = Some(token);
                } else if !matches!(token, "layout" | "row_major" | "column_major") {
                    break;
                } else if token == "layout" {
                    tokens.next();
                    if tokens.next()? != "(" {
                        return None;
                    }
                    while tokens.next()? != ")" {}
                    continue;
                }
                tokens.next();
            }
            let type_token = tokens.next()?;
            let type_name = if is_basic_type(type_token) {
                match precision_base(type_token) {
                    Some(base) => format!(
                        "{} {type_token}",
                        precision
                            .or_else(|| defaults.get(base).copied())
                            .unwrap_or("?")
                    ),
                    None => type_token.to_owned(),
                }
            } else {
                structs.get(type_token)?.clone()
            };
            let rest: Vec<&str> = tokens.collect();
            if rest.is_empty() {
                return None;
            }
            out.push_str(&format!("{type_name} {};", rest.join(" ")));
        }
        Some(out)
    }
}

/// Why a program whose stages compiled can't link (beyond what the driver checks), if it can't.
pub(crate) fn link_mismatch(
    vertex: &ShaderInterface,
    fragment: &ShaderInterface,
    essl3: bool,
    max_varying_vectors: u32,
) -> Option<String> {
    for (name, vertex_type) in &vertex.uniforms {
        if let Some(fragment_type) = fragment.uniforms.get(name) &&
            fragment_type != vertex_type
        {
            return Some(format!(
                "Uniform '{name}' differs between the shaders: {vertex_type} vs {fragment_type}"
            ));
        }
    }
    for (name, vertex_members) in &vertex.blocks {
        if let Some(fragment_members) = fragment.blocks.get(name) &&
            fragment_members != vertex_members
        {
            return Some(format!(
                "Uniform block '{name}' differs between the shaders"
            ));
        }
    }
    if !essl3 {
        let vertex_invariant = |name: &str| vertex.invariant_all || vertex.invariant.contains(name);
        if fragment.invariant.contains("gl_FragCoord") && !vertex_invariant("gl_Position") {
            return Some("gl_FragCoord is invariant but gl_Position is not".to_owned());
        }
        if fragment.invariant.contains("gl_PointCoord") && !vertex_invariant("gl_PointSize") {
            return Some("gl_PointCoord is invariant but gl_PointSize is not".to_owned());
        }
        let packed: Vec<&Varying> = fragment
            .varyings
            .iter()
            .filter(|(name, _)| {
                fragment.used.contains(*name) && vertex.varyings.contains_key(*name)
            })
            .map(|(_, varying)| varying)
            .collect();
        if !varyings_pack(&packed, max_varying_vectors) {
            return Some(
                "Varyings do not fit in MAX_VARYING_VECTORS (GLSL ES 1.00 A.7)".to_owned(),
            );
        }
    }
    None
}

/// GLSL ES 1.00 Appendix A.7 packing (as ANGLE's VariablePacker does it): whether the
/// varyings fit a grid of `max_vectors` rows of 4 columns.
fn varyings_pack(varyings: &[&Varying], max_vectors: u32) -> bool {
    // (sort order, columns, rows per element)
    let shape = |type_name: &str| -> Option<(u32, u32, u32)> {
        Some(match type_name {
            "mat4" => (0, 4, 4),
            "mat2" => (1, 4, 2),
            "vec4" => (2, 4, 1),
            "mat3" => (3, 3, 3),
            "vec3" => (4, 3, 1),
            "vec2" => (5, 2, 1),
            "float" => (6, 1, 1),
            _ => return None,
        })
    };
    let mut shaped = vec![];
    for varying in varyings {
        let Some((order, columns, rows)) = shape(&varying.type_name) else {
            // Not an ESSL 1.00 varying type: leave it to the driver.
            return true;
        };
        shaped.push((order, columns, rows * varying.array_size));
    }
    shaped.sort_by(|a, b| a.0.cmp(&b.0).then(b.2.cmp(&a.2)));
    let max = max_vectors as usize;
    let mut grid = vec![0u8; max];
    let fill = |grid: &mut Vec<u8>, top: usize, rows: usize, column: usize, columns: usize| {
        let bits = (((1u32 << columns) - 1) << column) as u8;
        for row in &mut grid[top..top + rows] {
            *row |= bits;
        }
    };
    let mut top = 0usize;
    let mut iter = shaped.iter().peekable();
    // 4-column variables from the top.
    while let Some(&&(_, 4, rows)) = iter.peek() {
        iter.next();
        top += rows as usize;
        if top > max {
            return false;
        }
    }
    fill(&mut grid, 0, top, 0, 4);
    // 3-column variables next.
    let mut rows3 = 0usize;
    while let Some(&&(_, 3, rows)) = iter.peek() {
        iter.next();
        rows3 += rows as usize;
    }
    if top + rows3 > max {
        return false;
    }
    fill(&mut grid, top, rows3, 0, 3);
    // 2-column variables in columns 0-1 from the top, then 2-3 from the bottom.
    let top2 = top + rows3;
    let available = max - top2;
    let (mut free01, mut free23) = (available, available);
    while let Some(&&(_, 2, rows)) = iter.peek() {
        iter.next();
        let rows = rows as usize;
        if rows <= free01 {
            free01 -= rows;
        } else if rows <= free23 {
            free23 -= rows;
        } else {
            return false;
        }
    }
    let (used01, used23) = (available - free01, available - free23);
    fill(&mut grid, top2, used01, 0, 2);
    fill(&mut grid, max - used23, used23, 2, 2);
    // 1-column variables in the smallest free run of any column that holds them.
    for &(_, _, rows) in iter {
        let rows = rows as usize;
        let mut best: Option<(usize, usize, usize)> = None; // (run size, column, top row)
        for column in 0..4 {
            let bit = 1u8 << column;
            let mut row = 0;
            while row < max {
                if grid[row] & bit != 0 {
                    row += 1;
                    continue;
                }
                let start = row;
                while row < max && grid[row] & bit == 0 {
                    row += 1;
                }
                let size = row - start;
                if size >= rows && best.is_none_or(|(best_size, ..)| size < best_size) {
                    best = Some((size, column, start));
                }
            }
        }
        let Some((_, column, start)) = best else {
            return false;
        };
        fill(&mut grid, start, rows, column, 1);
    }
    true
}
