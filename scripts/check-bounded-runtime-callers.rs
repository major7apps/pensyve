use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Clone, Debug, Eq, PartialEq)]
enum TokenKind {
    Ident(String),
    Punct(u8),
}

#[derive(Clone, Debug)]
struct Token {
    kind: TokenKind,
    line: usize,
}

fn is_ident_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic() || byte >= 0x80
}

fn is_ident_continue(byte: u8) -> bool {
    is_ident_start(byte) || byte.is_ascii_digit()
}

fn skip_quoted(bytes: &[u8], start: usize, line: &mut usize) -> Result<usize, String> {
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => {
                index += 1;
                if index < bytes.len() {
                    if bytes[index] == b'\n' {
                        *line += 1;
                    }
                    index += 1;
                }
            }
            b'"' => return Ok(index + 1),
            b'\n' => {
                *line += 1;
                index += 1;
            }
            _ => index += 1,
        }
    }
    Err("unterminated string literal".into())
}

fn utf8_char_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn char_literal_end(bytes: &[u8], quote: usize) -> Option<usize> {
    let first = *bytes.get(quote + 1)?;
    if first == b'\\' {
        let mut index = quote + 2;
        while index < bytes.len() && bytes[index] != b'\n' {
            if bytes[index] == b'\'' {
                return Some(index + 1);
            }
            index += 1;
        }
        return None;
    }
    let close = quote + 1 + utf8_char_len(first);
    (bytes.get(close) == Some(&b'\'')).then_some(close + 1)
}

fn raw_string_start(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    let prefix = if bytes.get(start) == Some(&b'r') {
        1
    } else if bytes.get(start..start + 2) == Some(b"br")
        || bytes.get(start..start + 2) == Some(b"cr")
    {
        2
    } else {
        return None;
    };
    let mut index = start + prefix;
    let mut hashes = 0;
    while bytes.get(index) == Some(&b'#') {
        hashes += 1;
        index += 1;
    }
    (bytes.get(index) == Some(&b'"')).then_some((index + 1, hashes))
}

fn skip_raw_string(
    bytes: &[u8],
    content_start: usize,
    hashes: usize,
    line: &mut usize,
) -> Result<usize, String> {
    let mut index = content_start;
    while index < bytes.len() {
        if bytes[index] == b'\n' {
            *line += 1;
        }
        if bytes[index] == b'"'
            && bytes
                .get(index + 1..index + 1 + hashes)
                .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
        {
            return Ok(index + 1 + hashes);
        }
        index += 1;
    }
    Err("unterminated raw string literal".into())
}

fn lex(source: &str) -> Result<Vec<Token>, (usize, String)> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    let mut line = 1;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            if bytes[index] == b'\n' {
                line += 1;
            }
            index += 1;
            continue;
        }
        if bytes.get(index..index + 2) == Some(b"//") {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if bytes.get(index..index + 2) == Some(b"/*") {
            let start_line = line;
            index += 2;
            let mut depth = 1_usize;
            while index < bytes.len() && depth > 0 {
                if bytes.get(index..index + 2) == Some(b"/*") {
                    depth += 1;
                    index += 2;
                } else if bytes.get(index..index + 2) == Some(b"*/") {
                    depth -= 1;
                    index += 2;
                } else {
                    if bytes[index] == b'\n' {
                        line += 1;
                    }
                    index += 1;
                }
            }
            if depth != 0 {
                return Err((start_line, "unterminated block comment".into()));
            }
            continue;
        }
        if let Some((content_start, hashes)) = raw_string_start(bytes, index) {
            index = skip_raw_string(bytes, content_start, hashes, &mut line)
                .map_err(|error| (line, error))?;
            continue;
        }
        if bytes.get(index..index + 2) == Some(b"r#")
            && bytes
                .get(index + 2)
                .is_some_and(|byte| is_ident_start(*byte))
        {
            let token_line = line;
            index += 2;
            let start = index;
            while index < bytes.len() && is_ident_continue(bytes[index]) {
                index += 1;
            }
            tokens.push(Token {
                kind: TokenKind::Ident(source[start..index].to_owned()),
                line: token_line,
            });
            continue;
        }
        if bytes[index] == b'"'
            || bytes.get(index..index + 2) == Some(b"b\"")
            || bytes.get(index..index + 2) == Some(b"c\"")
        {
            let quote = if bytes[index] == b'"' {
                index
            } else {
                index + 1
            };
            index = skip_quoted(bytes, quote, &mut line).map_err(|error| (line, error))?;
            continue;
        }
        if bytes[index] == b'\'' {
            if let Some(end) = char_literal_end(bytes, index) {
                index = end;
                continue;
            }
        } else if bytes.get(index..index + 2) == Some(b"b'") {
            if let Some(end) = char_literal_end(bytes, index + 1) {
                index = end;
                continue;
            }
        }
        if is_ident_start(bytes[index]) {
            let token_line = line;
            let start = index;
            index += 1;
            while index < bytes.len() && is_ident_continue(bytes[index]) {
                index += 1;
            }
            tokens.push(Token {
                kind: TokenKind::Ident(source[start..index].to_owned()),
                line: token_line,
            });
            continue;
        }
        tokens.push(Token {
            kind: TokenKind::Punct(bytes[index]),
            line,
        });
        index += 1;
    }
    Ok(tokens)
}

fn punct(token: &Token, expected: u8) -> bool {
    token.kind == TokenKind::Punct(expected)
}

fn ident(token: &Token, expected: &str) -> bool {
    matches!(&token.kind, TokenKind::Ident(value) if value == expected)
}

fn exact_cfg_test(tokens: &[Token], index: usize) -> bool {
    tokens.get(index).is_some_and(|token| punct(token, b'#'))
        && tokens
            .get(index + 1)
            .is_some_and(|token| punct(token, b'['))
        && tokens
            .get(index + 2)
            .is_some_and(|token| ident(token, "cfg"))
        && tokens
            .get(index + 3)
            .is_some_and(|token| punct(token, b'('))
        && tokens
            .get(index + 4)
            .is_some_and(|token| ident(token, "test"))
        && tokens
            .get(index + 5)
            .is_some_and(|token| punct(token, b')'))
        && tokens
            .get(index + 6)
            .is_some_and(|token| punct(token, b']'))
}

fn delimiter_end(tokens: &[Token], start: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0_usize;
    for (offset, token) in tokens[start..].iter().enumerate() {
        if punct(token, open) {
            depth += 1;
        } else if punct(token, close) {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(start + offset + 1);
            }
        }
    }
    None
}

fn skip_attributes(tokens: &[Token], mut index: usize) -> Result<usize, String> {
    while tokens.get(index).is_some_and(|token| punct(token, b'#'))
        && tokens
            .get(index + 1)
            .is_some_and(|token| punct(token, b'['))
    {
        index = delimiter_end(tokens, index + 1, b'[', b']')
            .ok_or_else(|| "unterminated attribute after #[cfg(test)]".to_string())?;
    }
    Ok(index)
}

fn declaration_uses_semicolon(tokens: &[Token], start: usize) -> bool {
    let mut index = start;
    if tokens.get(index).is_some_and(|token| ident(token, "pub")) {
        index += 1;
        if tokens.get(index).is_some_and(|token| punct(token, b'(')) {
            index = delimiter_end(tokens, index, b'(', b')').unwrap_or(index);
        }
    }
    while tokens.get(index).is_some_and(|token| {
        matches!(&token.kind, TokenKind::Ident(value) if matches!(value.as_str(), "unsafe" | "async" | "default" | "extern"))
    }) {
        index += 1;
    }
    if tokens.get(index).is_some_and(|token| ident(token, "const")) {
        return !tokens[index + 1..tokens.len().min(index + 6)]
            .iter()
            .any(|token| ident(token, "fn"));
    }
    tokens.get(index).is_some_and(|token| {
        matches!(&token.kind, TokenKind::Ident(value) if matches!(value.as_str(), "use" | "type" | "static" | "let"))
    })
}

fn declaration_uses_comma(tokens: &[Token], start: usize) -> bool {
    let mut index = start;
    if tokens.get(index).is_some_and(|token| ident(token, "pub")) {
        index += 1;
        if tokens.get(index).is_some_and(|token| punct(token, b'(')) {
            index = delimiter_end(tokens, index, b'(', b')').unwrap_or(index);
        }
    }
    matches!(
        tokens.get(index).map(|token| &token.kind),
        Some(TokenKind::Ident(_))
    ) && tokens
        .get(index + 1)
        .is_some_and(|token| punct(token, b':'))
}

fn cfg_item_end(tokens: &[Token], after_cfg: usize) -> Result<usize, String> {
    let start = skip_attributes(tokens, after_cfg)?;
    if start >= tokens.len() {
        return Err("#[cfg(test)] has no attached item".into());
    }
    let semicolon_item = declaration_uses_semicolon(tokens, start);
    let comma_item = declaration_uses_comma(tokens, start);
    let mut stack = Vec::new();
    let mut angle_depth = 0_usize;
    for (offset, token) in tokens[start..].iter().enumerate() {
        let index = start + offset;
        match token.kind {
            TokenKind::Punct(b'(') => stack.push(b')'),
            TokenKind::Punct(b'[') => stack.push(b']'),
            TokenKind::Punct(b'{') if semicolon_item || !stack.is_empty() || angle_depth > 0 => {
                stack.push(b'}');
            }
            TokenKind::Punct(b'{') => {
                return delimiter_end(tokens, index, b'{', b'}')
                    .ok_or_else(|| "unterminated #[cfg(test)] item body".into());
            }
            TokenKind::Punct(close @ (b')' | b']' | b'}')) => {
                if stack.pop() != Some(close) {
                    return Err("unbalanced delimiter in #[cfg(test)] item".into());
                }
            }
            TokenKind::Punct(b'<') if stack.is_empty() && !semicolon_item => angle_depth += 1,
            TokenKind::Punct(b'>') if stack.is_empty() && angle_depth > 0 => angle_depth -= 1,
            TokenKind::Punct(b';') if stack.is_empty() && angle_depth == 0 => return Ok(index + 1),
            TokenKind::Punct(b',') if comma_item && stack.is_empty() && angle_depth == 0 => {
                return Ok(index + 1);
            }
            _ => {}
        }
    }
    Err("unterminated #[cfg(test)] item".into())
}

fn scan_file(source: &str) -> Result<Vec<(usize, String)>, (usize, String)> {
    let tokens = lex(source)?;
    let mut violations = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        if exact_cfg_test(&tokens, index) {
            index =
                cfg_item_end(&tokens, index + 7).map_err(|error| (tokens[index].line, error))?;
            continue;
        }
        if let TokenKind::Ident(value) = &tokens[index].kind {
            if value == "VectorIndex"
                || value == "get_all_memories_by_namespace"
                || value == "get_all_memories_by_namespace_including_superseded"
            {
                violations.push((tokens[index].line, value.clone()));
            }
        }
        index += 1;
    }
    Ok(violations)
}

fn collect_rust_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let metadata =
        fs::metadata(root).map_err(|error| format!("cannot access {}: {error}", root.display()))?;
    if !metadata.is_dir() {
        return Err(format!("input is not a directory: {}", root.display()));
    }
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    let mut visited = HashSet::new();
    while let Some(directory) = pending.pop() {
        let canonical = fs::canonicalize(&directory)
            .map_err(|error| format!("cannot resolve {}: {error}", directory.display()))?;
        if !visited.insert(canonical) {
            continue;
        }
        let entries = fs::read_dir(&directory)
            .map_err(|error| format!("cannot read directory {}: {error}", directory.display()))?;
        let mut paths = entries
            .map(|entry| {
                entry
                    .map(|value| value.path())
                    .map_err(|error| format!("cannot enumerate {}: {error}", directory.display()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        paths.sort();
        for path in paths.into_iter().rev() {
            let metadata = fs::metadata(&path)
                .map_err(|error| format!("cannot access {}: {error}", path.display()))?;
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() && path.extension().is_some_and(|value| value == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(format!(
            "input contains no Rust source files: {}",
            root.display()
        ));
    }
    Ok(files)
}

fn run() -> Result<bool, String> {
    let roots = env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if roots.is_empty() {
        return Err("no source directories supplied".into());
    }
    let mut found = false;
    for root in roots {
        for path in collect_rust_files(&root)? {
            let bytes = fs::read(&path)
                .map_err(|error| format!("cannot read Rust source {}: {error}", path.display()))?;
            let source = String::from_utf8(bytes)
                .map_err(|error| format!("Rust source is not UTF-8 {}: {error}", path.display()))?;
            match scan_file(&source) {
                Ok(violations) => {
                    for (line, identifier) in violations {
                        eprintln!(
                            "{}:{line}: shipping runtime contains forbidden identifier `{identifier}`",
                            path.display()
                        );
                        found = true;
                    }
                }
                Err((line, error)) => {
                    return Err(format!(
                        "{}:{line}: cannot scan Rust source: {error}",
                        path.display()
                    ));
                }
            }
        }
    }
    Ok(found)
}

fn main() -> ExitCode {
    match run() {
        Ok(false) => ExitCode::SUCCESS,
        Ok(true) => {
            eprintln!(
                "shipping runtime still contains corpus hydration or a resident vector index"
            );
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("bounded runtime guard failed: {error}");
            ExitCode::FAILURE
        }
    }
}
