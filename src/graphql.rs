//! A small GraphQL executor for the subset the kit exercises: `viewer`,
//! `repository`, `node`, `__type`, the issue/pull-request connections with
//! cursor pagination, and the five mutations. Supports variables, aliases,
//! inline fragments, named fragments, and batched requests.

use crate::http::{Request, Response};
use crate::json::{obj, Json};
use crate::rest::next_request_id;
use crate::store::{diff_trees, pull_mergeable, squash_merge, IssueState, Repo, Store};
use crate::util::{base64_decode, base64_encode, iso_from_epoch};
use std::collections::HashMap;
use std::sync::RwLock;

pub fn handle(store: &RwLock<Store>, req: &Request) -> Response {
    let body = match crate::json::parse(&String::from_utf8_lossy(&req.body)) {
        Ok(body) => body,
        Err(_) => {
            return Response::new(
                400,
                br#"{"errors":[{"message":"malformed JSON body"}]}"#.to_vec(),
            )
            .header("content-type", "application/json; charset=utf-8")
            .header("x-github-request-id", &next_request_id())
        }
    };
    let result = match &body {
        Json::Arr(operations) => {
            let mut guard = store.write().unwrap();
            let results: Vec<Json> = operations
                .iter()
                .map(|op| execute_one(&mut guard, op))
                .collect();
            Json::Arr(results)
        }
        Json::Obj(_) => {
            let mut guard = store.write().unwrap();
            execute_one(&mut guard, &body)
        }
        _ => obj(vec![(
            "errors",
            Json::Arr(vec![obj(vec![(
                "message",
                Json::Str("expected a JSON object or array".to_string()),
            )])]),
        )]),
    };
    Response::new(200, result.stringify().into_bytes())
        .header("content-type", "application/json; charset=utf-8")
        .header("x-github-request-id", &next_request_id())
}

fn execute_one(store: &mut Store, operation: &Json) -> Json {
    let Some(query) = operation.get("query").and_then(|v| v.as_str()) else {
        return obj(vec![(
            "errors",
            Json::Arr(vec![obj(vec![(
                "message",
                Json::Str("query is required".to_string()),
            )])]),
        )]);
    };
    let variables = operation.get("variables").cloned().unwrap_or(Json::Null);
    let operation_name = operation
        .get("operationName")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let mut parser = match Parser::new(query) {
        Ok(parser) => parser,
        Err(message) => {
            return obj(vec![(
                "errors",
                Json::Arr(vec![obj(vec![("message", Json::Str(message))])]),
            )]);
        }
    };
    let document = match parser.parse_document() {
        Ok(document) => document,
        Err(message) => {
            return obj(vec![(
                "errors",
                Json::Arr(vec![obj(vec![("message", Json::Str(message))])]),
            )]);
        }
    };
    let mut executor = Executor {
        store,
        variables: &variables,
        errors: Vec::new(),
    };
    match executor.execute(&document, operation_name.as_deref()) {
        Some(data) => {
            if executor.errors.is_empty() {
                obj(vec![("data", data)])
            } else {
                obj(vec![("data", data), ("errors", Json::Arr(executor.errors))])
            }
        }
        None => obj(vec![("errors", Json::Arr(executor.errors))]),
    }
}

// ---------------------------------------------------------------------------
// Lexer.

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Name(String),
    Int(i64),
    Float(f64),
    Str(String),
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Colon,
    Bang,
    Dollar,
    Eq,
    Spread,
    At,
    Pipe,
    Amp,
}

fn lex(input: &str) -> Result<Vec<Token>, String> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            ' ' | '\t' | '\n' | '\r' | ',' => i += 1,
            '#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            '{' => {
                tokens.push(Token::LBrace);
                i += 1;
            }
            '}' => {
                tokens.push(Token::RBrace);
                i += 1;
            }
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            '[' => {
                tokens.push(Token::LBracket);
                i += 1;
            }
            ']' => {
                tokens.push(Token::RBracket);
                i += 1;
            }
            ':' => {
                tokens.push(Token::Colon);
                i += 1;
            }
            '!' => {
                tokens.push(Token::Bang);
                i += 1;
            }
            '$' => {
                tokens.push(Token::Dollar);
                i += 1;
            }
            '=' => {
                tokens.push(Token::Eq);
                i += 1;
            }
            '@' => {
                tokens.push(Token::At);
                i += 1;
            }
            '|' => {
                tokens.push(Token::Pipe);
                i += 1;
            }
            '&' => {
                tokens.push(Token::Amp);
                i += 1;
            }
            '.' => {
                if input[i..].starts_with("...") {
                    tokens.push(Token::Spread);
                    i += 3;
                } else {
                    return Err("unexpected '.'".to_string());
                }
            }
            '"' => {
                let (text, consumed) = lex_string(input, i)?;
                tokens.push(Token::Str(text));
                i += consumed;
            }
            '-' | '0'..='9' => {
                let start = i;
                if bytes[i] == b'-' {
                    i += 1;
                }
                while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                    i += 1;
                }
                let mut is_float = false;
                if i < bytes.len() && bytes[i] == b'.' {
                    is_float = true;
                    i += 1;
                    while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                        i += 1;
                    }
                }
                if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
                    is_float = true;
                    i += 1;
                    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
                        i += 1;
                    }
                    while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                        i += 1;
                    }
                }
                let text = &input[start..i];
                if is_float {
                    tokens.push(Token::Float(
                        text.parse().map_err(|_| "bad float".to_string())?,
                    ));
                } else {
                    tokens.push(Token::Int(text.parse().map_err(|_| "bad int".to_string())?));
                }
            }
            _ if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < bytes.len()
                    && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] == b'_')
                {
                    i += 1;
                }
                tokens.push(Token::Name(input[start..i].to_string()));
            }
            _ => return Err(format!("unexpected character '{c}'")),
        }
    }
    Ok(tokens)
}

fn lex_string(input: &str, start: usize) -> Result<(String, usize), String> {
    let bytes = input.as_bytes();
    let mut i = start + 1;
    let mut out = String::new();
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Ok((out, i + 1 - start)),
            b'\\' => {
                i += 1;
                if i >= bytes.len() {
                    break;
                }
                match bytes[i] {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000C}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let hex = input.get(i + 1..i + 5).ok_or("bad \\u escape")?;
                        let code = u32::from_str_radix(hex, 16).map_err(|_| "bad \\u escape")?;
                        let ch = char::from_u32(code).ok_or("bad \\u escape")?;
                        out.push(ch);
                        i += 4;
                    }
                    _ => return Err("bad string escape".to_string()),
                }
                i += 1;
            }
            _ => {
                // Copy the next UTF-8 character (a truncated tail is an
                // unterminated string, not a panic: panic aborts the process).
                let width = utf8_len(bytes[i]);
                if i + width > bytes.len() {
                    return Err("unterminated string".to_string());
                }
                out.push_str(&input[i..i + width]);
                i += width;
            }
        }
    }
    Err("unterminated string".to_string())
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

// ---------------------------------------------------------------------------
// AST + parser.

#[derive(Clone, Debug)]
enum Value {
    Variable(String),
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Null,
    Enum(String),
    List(Vec<Value>),
    Object(Vec<(String, Value)>),
}

#[derive(Clone, Debug)]
struct Field {
    alias: Option<String>,
    name: String,
    arguments: Vec<(String, Value)>,
    selection: Vec<Selection>,
}

#[derive(Clone, Debug)]
enum Selection {
    Field(Field),
    InlineFragment {
        type_condition: Option<String>,
        selection: Vec<Selection>,
    },
    FragmentSpread(String),
}

#[derive(Clone, Debug)]
struct Operation {
    kind: String, // "query" | "mutation"
    name: Option<String>,
    selection: Vec<Selection>,
}

#[derive(Clone, Debug)]
struct Document {
    operations: Vec<Operation>,
    fragments: HashMap<String, Vec<Selection>>,
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(input: &str) -> Result<Self, String> {
        Ok(Parser {
            tokens: lex(input)?,
            pos: 0,
        })
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn expect(&mut self, expected: &Token) -> Result<(), String> {
        match self.next() {
            Some(token) if &token == expected => Ok(()),
            other => Err(format!("expected {expected:?}, found {other:?}")),
        }
    }

    fn parse_document(&mut self) -> Result<Document, String> {
        let mut operations = Vec::new();
        let mut fragments = HashMap::new();
        while self.peek().is_some() {
            match self.peek() {
                Some(Token::LBrace) => {
                    let selection = self.parse_selection_set()?;
                    operations.push(Operation {
                        kind: "query".to_string(),
                        name: None,
                        selection,
                    });
                }
                Some(Token::Name(name)) if name == "query" || name == "mutation" => {
                    let kind = name.clone();
                    self.next();
                    let op_name = match self.peek() {
                        Some(Token::Name(_)) => {
                            if let Some(Token::Name(n)) = self.next() {
                                Some(n)
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    // Variable definitions: ($x: Type = default, ...).
                    if self.peek() == Some(&Token::LParen) {
                        self.skip_balanced(Token::LParen, Token::RParen)?;
                    }
                    // Directives: @name(args...).
                    while self.peek() == Some(&Token::At) {
                        self.next();
                        self.parse_name()?;
                        if self.peek() == Some(&Token::LParen) {
                            self.skip_balanced(Token::LParen, Token::RParen)?;
                        }
                    }
                    let selection = self.parse_selection_set()?;
                    operations.push(Operation {
                        kind,
                        name: op_name,
                        selection,
                    });
                }
                Some(Token::Name(name)) if name == "fragment" => {
                    self.next();
                    let frag_name = self.parse_name()?;
                    // `on Type`
                    let on = self.parse_name()?;
                    if on != "on" {
                        return Err("expected 'on' in fragment".to_string());
                    }
                    self.parse_name()?; // the type name
                    let selection = self.parse_selection_set()?;
                    fragments.insert(frag_name, selection);
                }
                other => return Err(format!("unexpected token at document level: {other:?}")),
            }
        }
        if operations.is_empty() {
            return Err("no operation in document".to_string());
        }
        Ok(Document {
            operations,
            fragments,
        })
    }

    fn parse_name(&mut self) -> Result<String, String> {
        match self.next() {
            Some(Token::Name(name)) => Ok(name),
            other => Err(format!("expected a name, found {other:?}")),
        }
    }

    fn skip_balanced(&mut self, open: Token, close: Token) -> Result<(), String> {
        self.expect(&open)?;
        let mut depth = 1;
        while depth > 0 {
            match self.next() {
                Some(token) if token == open => depth += 1,
                Some(token) if token == close => depth -= 1,
                Some(_) => {}
                None => return Err("unbalanced parentheses".to_string()),
            }
        }
        Ok(())
    }

    fn parse_selection_set(&mut self) -> Result<Vec<Selection>, String> {
        self.expect(&Token::LBrace)?;
        let mut selections = Vec::new();
        while self.peek() != Some(&Token::RBrace) {
            if self.peek().is_none() {
                return Err("unterminated selection set".to_string());
            }
            selections.push(self.parse_selection()?);
        }
        self.expect(&Token::RBrace)?;
        Ok(selections)
    }

    fn parse_selection(&mut self) -> Result<Selection, String> {
        match self.peek() {
            Some(Token::Spread) => {
                self.next();
                match self.peek() {
                    Some(Token::Name(name)) if name == "on" => {
                        self.next();
                        let type_name = self.parse_name()?;
                        let selection = self.parse_selection_set()?;
                        Ok(Selection::InlineFragment {
                            type_condition: Some(type_name),
                            selection,
                        })
                    }
                    Some(Token::Name(name)) => {
                        let frag = name.clone();
                        self.next();
                        Ok(Selection::FragmentSpread(frag))
                    }
                    Some(Token::LBrace) => {
                        let selection = self.parse_selection_set()?;
                        Ok(Selection::InlineFragment {
                            type_condition: None,
                            selection,
                        })
                    }
                    other => Err(format!("bad fragment spread: {other:?}")),
                }
            }
            Some(Token::Name(_)) => {
                let first = self.parse_name()?;
                let (alias, name) = if self.peek() == Some(&Token::Colon) {
                    self.next();
                    (Some(first), self.parse_name()?)
                } else {
                    (None, first)
                };
                let mut arguments = Vec::new();
                if self.peek() == Some(&Token::LParen) {
                    self.next();
                    while self.peek() != Some(&Token::RParen) {
                        let arg_name = self.parse_name()?;
                        self.expect(&Token::Colon)?;
                        let value = self.parse_value()?;
                        arguments.push((arg_name, value));
                    }
                    self.expect(&Token::RParen)?;
                }
                // Directives (ignored: @include/@skip are not exercised).
                while self.peek() == Some(&Token::At) {
                    self.next();
                    self.parse_name()?;
                    if self.peek() == Some(&Token::LParen) {
                        self.skip_balanced(Token::LParen, Token::RParen)?;
                    }
                }
                let selection = if self.peek() == Some(&Token::LBrace) {
                    self.parse_selection_set()?
                } else {
                    Vec::new()
                };
                Ok(Selection::Field(Field {
                    alias,
                    name,
                    arguments,
                    selection,
                }))
            }
            other => Err(format!("unexpected token in selection set: {other:?}")),
        }
    }

    fn parse_value(&mut self) -> Result<Value, String> {
        match self.next() {
            Some(Token::Dollar) => Ok(Value::Variable(self.parse_name()?)),
            Some(Token::Int(n)) => Ok(Value::Int(n)),
            Some(Token::Float(f)) => Ok(Value::Float(f)),
            Some(Token::Str(s)) => Ok(Value::Str(s)),
            Some(Token::Name(name)) => match name.as_str() {
                "true" => Ok(Value::Bool(true)),
                "false" => Ok(Value::Bool(false)),
                "null" => Ok(Value::Null),
                _ => Ok(Value::Enum(name)),
            },
            Some(Token::LBracket) => {
                let mut items = Vec::new();
                while self.peek() != Some(&Token::RBracket) {
                    items.push(self.parse_value()?);
                }
                self.expect(&Token::RBracket)?;
                Ok(Value::List(items))
            }
            Some(Token::LBrace) => {
                let mut fields = Vec::new();
                while self.peek() != Some(&Token::RBrace) {
                    let key = self.parse_name()?;
                    self.expect(&Token::Colon)?;
                    fields.push((key, self.parse_value()?));
                }
                self.expect(&Token::RBrace)?;
                Ok(Value::Object(fields))
            }
            other => Err(format!("unexpected value token: {other:?}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Executor.

struct Executor<'a> {
    store: &'a mut Store,
    variables: &'a Json,
    errors: Vec<Json>,
}

#[derive(Clone, Debug)]
enum Node {
    Query,
    Mutation,
    User(String),
    Repository(u32),
    Issue(u32, u32), // (repo db id, number)
    Pull(u32, u32),
    Branch(String),
    /// (repo db id, issue numbers in query order, cursor number, first)
    IssueConnection(u32, Vec<u32>, Option<u32>, usize),
    /// (repo db id, pull numbers, first)
    PullConnection(u32, Vec<u32>, usize),
    /// (repo db id, pull number, file paths, first)
    FileConnection(u32, u32, Vec<String>, usize),
    Comment(u64, String),
    CommentEdge(u64, String),
    PageInfo(bool, Option<String>),
    CreateIssue(u32, u32),
    CloseIssue(u32, u32),
    CreatePull(u32, u32),
    MergePull(u32, u32),
    AddComment(u64, String),
    Type(String),
}

impl<'a> Executor<'a> {
    fn execute(&mut self, document: &Document, operation_name: Option<&str>) -> Option<Json> {
        let operation = if document.operations.len() == 1 {
            &document.operations[0]
        } else {
            let name = operation_name?;
            document
                .operations
                .iter()
                .find(|op| op.name.as_deref() == Some(name))?
        };
        let root = if operation.kind == "mutation" {
            Node::Mutation
        } else {
            Node::Query
        };
        let mut out = Vec::new();
        self.resolve_selections(document, root, &operation.selection, &mut out);
        Some(Json::Obj(out))
    }

    fn resolve_selections(
        &mut self,
        document: &Document,
        node: Node,
        selections: &[Selection],
        out: &mut Vec<(String, Json)>,
    ) {
        for selection in selections {
            match selection {
                Selection::Field(field) => {
                    let key = field.alias.clone().unwrap_or_else(|| field.name.clone());
                    let value = self.resolve_field(document, &node, field);
                    out.push((key, value));
                }
                Selection::InlineFragment {
                    type_condition,
                    selection,
                } => {
                    if type_condition
                        .as_deref()
                        .is_some_and(|t| t != self.node_type_name(&node))
                    {
                        continue;
                    }
                    self.resolve_selections(document, node.clone(), selection, out);
                }
                Selection::FragmentSpread(name) => {
                    if let Some(selection) = document.fragments.get(name).cloned() {
                        self.resolve_selections(document, node.clone(), &selection, out);
                    }
                }
            }
        }
    }

    fn node_type_name(&self, node: &Node) -> &str {
        match node {
            Node::Query => "Query",
            Node::Mutation => "Mutation",
            Node::User(_) => "User",
            Node::Repository(_) => "Repository",
            Node::Issue(_, _) => "Issue",
            Node::Pull(_, _) => "PullRequest",
            Node::Branch(_) => "Ref",
            Node::IssueConnection(_, _, _, _) => "IssueConnection",
            Node::PullConnection(_, _, _) => "PullRequestConnection",
            Node::FileConnection(_, _, _, _) => "PullRequestChangedFileConnection",
            Node::Comment(_, _) => "IssueComment",
            Node::CommentEdge(_, _) => "IssueCommentEdge",
            Node::PageInfo(_, _) => "PageInfo",
            Node::CreateIssue(_, _) => "CreateIssuePayload",
            Node::CloseIssue(_, _) => "CloseIssuePayload",
            Node::CreatePull(_, _) => "CreatePullRequestPayload",
            Node::MergePull(_, _) => "MergePullRequestPayload",
            Node::AddComment(_, _) => "AddCommentPayload",
            Node::Type(_) => "__Type",
        }
    }

    // ---- argument helpers ----

    fn arg_value(&self, field: &Field, name: &str) -> Option<Json> {
        field
            .arguments
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| self.value_to_json(v))
    }

    fn arg_str(&self, field: &Field, name: &str) -> Option<String> {
        self.arg_value(field, name)?.as_str().map(|s| s.to_string())
    }

    fn arg_int(&self, field: &Field, name: &str) -> Option<i64> {
        self.arg_value(field, name)?.as_i64()
    }

    fn value_to_json(&self, value: &Value) -> Json {
        match value {
            Value::Variable(name) => self.variables.get(name).cloned().unwrap_or(Json::Null),
            Value::Int(n) => Json::Int(*n),
            Value::Float(f) => Json::Float(*f),
            Value::Str(s) => Json::Str(s.clone()),
            Value::Bool(b) => Json::Bool(*b),
            Value::Null => Json::Null,
            Value::Enum(name) => Json::Str(name.clone()),
            Value::List(items) => Json::Arr(items.iter().map(|v| self.value_to_json(v)).collect()),
            Value::Object(fields) => Json::Obj(
                fields
                    .iter()
                    .map(|(k, v)| (k.clone(), self.value_to_json(v)))
                    .collect(),
            ),
        }
    }

    fn error(&mut self, message: String) {
        self.errors.push(obj(vec![("message", Json::Str(message))]));
    }

    // ---- field resolution ----

    fn resolve_field(&mut self, document: &Document, node: &Node, field: &Field) -> Json {
        match node {
            Node::Query => self.resolve_query(document, field),
            Node::Mutation => self.resolve_mutation(document, field),
            Node::User(login) => self.resolve_user(document, login, field),
            Node::Repository(db_id) => self.resolve_repository(document, *db_id, field),
            Node::Issue(db_id, number) => self.resolve_issue(document, *db_id, *number, field),
            Node::Pull(db_id, number) => self.resolve_pull(document, *db_id, *number, field),
            Node::Branch(name) => self.resolve_branch(document, name, field),
            Node::IssueConnection(db_id, numbers, after, first) => {
                self.resolve_issue_connection(document, *db_id, numbers, *after, *first, field)
            }
            Node::PullConnection(db_id, numbers, first) => {
                self.resolve_pull_connection(document, *db_id, numbers, *first, field)
            }
            Node::FileConnection(db_id, number, paths, first) => {
                self.resolve_file_connection(document, *db_id, *number, paths, *first, field)
            }
            Node::Comment(id, body) => self.resolve_comment(document, *id, body, field),
            Node::CommentEdge(id, body) => self.resolve_comment_edge(document, *id, body, field),
            Node::PageInfo(has_next, end_cursor) => {
                self.resolve_page_info(field, *has_next, end_cursor.clone())
            }
            Node::CreateIssue(db_id, number) => {
                self.resolve_create_issue(document, *db_id, *number, field)
            }
            Node::CloseIssue(db_id, number) => {
                self.resolve_close_issue(document, *db_id, *number, field)
            }
            Node::CreatePull(db_id, number) => {
                self.resolve_create_pull(document, *db_id, *number, field)
            }
            Node::MergePull(db_id, number) => {
                self.resolve_merge_pull(document, *db_id, *number, field)
            }
            Node::AddComment(id, body) => self.resolve_add_comment(document, *id, body, field),
            Node::Type(name) => self.resolve_type(document, name, field),
        }
    }

    fn sub(&mut self, document: &Document, node: Node, field: &Field) -> Json {
        if field.selection.is_empty() {
            return Json::Null;
        }
        let mut out = Vec::new();
        self.resolve_selections(document, node, &field.selection, &mut out);
        Json::Obj(out)
    }

    fn unknown_field(&mut self, node: &Node, field: &Field) -> Json {
        let message = format!(
            "Cannot query field \"{}\" on type \"{}\".",
            field.name,
            self.node_type_name(node)
        );
        self.error(message);
        Json::Null
    }

    fn resolve_query(&mut self, document: &Document, field: &Field) -> Json {
        match field.name.as_str() {
            "viewer" => self.sub(document, Node::User(self.store.viewer.login.clone()), field),
            "repository" => {
                let owner = self.arg_str(field, "owner");
                let name = self.arg_str(field, "name");
                match (owner, name) {
                    (Some(owner), Some(name)) => match self.store.get_repo(&owner, &name) {
                        Some(repo) => self.sub(document, Node::Repository(repo.db_id), field),
                        None => Json::Null,
                    },
                    _ => Json::Null,
                }
            }
            "node" => {
                let Some(id) = self.arg_str(field, "id") else {
                    return Json::Null;
                };
                match self.store.decode_gid(&id) {
                    Some((kind, db_id, number)) => match kind.as_str() {
                        "Repository" => self.sub(document, Node::Repository(db_id), field),
                        "Issue" => self.sub(document, Node::Issue(db_id, number), field),
                        "PullRequest" => self.sub(document, Node::Pull(db_id, number), field),
                        _ => Json::Null,
                    },
                    None => Json::Null,
                }
            }
            "__type" => {
                let Some(name) = self.arg_str(field, "name") else {
                    return Json::Null;
                };
                self.sub(document, Node::Type(name), field)
            }
            "__schema" => self.sub(document, Node::Type("__schema".to_string()), field),
            _ => self.unknown_field(&Node::Query, field),
        }
    }

    fn resolve_user(&mut self, _document: &Document, login: &str, field: &Field) -> Json {
        match field.name.as_str() {
            "login" => Json::Str(login.to_string()),
            "id" => Json::Str(base64_encode(format!("User:{login}").as_bytes())),
            "name" => Json::Str(self.store.viewer.name.clone()),
            _ => self.unknown_field(&Node::User(login.to_string()), field),
        }
    }

    fn repo_by_db_id(&self, db_id: u32) -> Option<&Repo> {
        self.store.repos.values().find(|repo| repo.db_id == db_id)
    }

    fn resolve_repository(&mut self, document: &Document, db_id: u32, field: &Field) -> Json {
        let Some(repo) = self.repo_by_db_id(db_id) else {
            return Json::Null;
        };
        let gid = self.store.repo_gid(repo);
        match field.name.as_str() {
            "id" => Json::Str(gid),
            "databaseId" => Json::Int(repo.db_id as i64),
            "name" => Json::Str(repo.name.clone()),
            "nameWithOwner" => Json::Str(repo.full_name()),
            "owner" => self.sub(document, Node::User(repo.owner.clone()), field),
            "defaultBranchRef" => match repo.default_tip() {
                Some(_) => self.sub(document, Node::Branch(repo.default_branch.clone()), field),
                None => Json::Null,
            },
            "viewerPermission" => Json::Str("ADMIN".to_string()),
            "hasIssuesEnabled" => Json::Bool(true),
            "issue" => {
                let number = self.arg_int(field, "number").unwrap_or(0) as u32;
                if repo.issues.iter().any(|issue| issue.number == number) {
                    self.sub(document, Node::Issue(db_id, number), field)
                } else {
                    Json::Null
                }
            }
            "issues" => {
                let numbers = self.filtered_issue_numbers(repo, field);
                // Pagination arguments live on the connection field itself.
                let after = self.arg_str(field, "after").and_then(|cursor| {
                    base64_decode(&cursor)
                        .and_then(|bytes| String::from_utf8(bytes).ok())
                        .and_then(|text| {
                            text.strip_prefix("cursor:")
                                .map(|n| n.parse::<u32>().ok())
                                .unwrap_or(None)
                        })
                });
                let first = self.arg_int(field, "first").unwrap_or(100).max(0) as usize;
                self.sub(
                    document,
                    Node::IssueConnection(db_id, numbers, after, first),
                    field,
                )
            }
            "pullRequest" => {
                let number = self.arg_int(field, "number").unwrap_or(0) as u32;
                if repo.pulls.iter().any(|pull| pull.number == number) {
                    self.sub(document, Node::Pull(db_id, number), field)
                } else {
                    Json::Null
                }
            }
            "pullRequests" => {
                let numbers: Vec<u32> = repo.pulls.iter().map(|pull| pull.number).collect();
                let first = self.arg_int(field, "first").unwrap_or(100).max(0) as usize;
                self.sub(document, Node::PullConnection(db_id, numbers, first), field)
            }
            _ => self.unknown_field(&Node::Repository(db_id), field),
        }
    }

    /// Issue numbers after `states` + `orderBy` filtering (before `first`).
    fn filtered_issue_numbers(&self, repo: &Repo, field: &Field) -> Vec<u32> {
        let states: Vec<String> = self
            .arg_value(field, "states")
            .and_then(|v| {
                v.as_arr().map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
            })
            .unwrap_or_default();
        let mut issues: Vec<&crate::store::Issue> = repo
            .issues
            .iter()
            .filter(|issue| {
                states.is_empty()
                    || states.iter().any(|state| match state.as_str() {
                        "OPEN" => issue.state == IssueState::Open,
                        "CLOSED" => issue.state == IssueState::Closed,
                        _ => true,
                    })
            })
            .collect();
        // orderBy: {field: CREATED_AT, direction: DESC|ASC} — created order
        // is the number order.
        let desc = self
            .arg_value(field, "orderBy")
            .and_then(|v| {
                v.get("direction")
                    .and_then(|d| d.as_str())
                    .map(|d| d == "DESC")
            })
            .unwrap_or(false);
        issues.sort_by_key(|issue| issue.number);
        if desc {
            issues.reverse();
        }
        issues.into_iter().map(|issue| issue.number).collect()
    }

    fn resolve_issue(
        &mut self,
        document: &Document,
        db_id: u32,
        number: u32,
        field: &Field,
    ) -> Json {
        let Some(repo) = self.repo_by_db_id(db_id) else {
            return Json::Null;
        };
        let Some(issue) = repo.issues.iter().find(|issue| issue.number == number) else {
            return Json::Null;
        };
        let gid = self.store.issue_gid(repo, number);
        let state = issue.state.as_str();
        match field.name.as_str() {
            "id" => Json::Str(gid),
            "number" => Json::Int(number as i64),
            "title" => Json::Str(issue.title.clone()),
            "body" => Json::Str(issue.body.clone()),
            "state" => Json::Str(state.to_uppercase()),
            "createdAt" => Json::Str(iso_from_epoch(
                crate::scenario::SCENARIO_EPOCH_SEC + issue.created_seq,
            )),
            "author" => self.sub(document, Node::User(self.store.viewer.login.clone()), field),
            _ => self.unknown_field(&Node::Issue(db_id, number), field),
        }
    }

    fn resolve_pull(
        &mut self,
        document: &Document,
        db_id: u32,
        number: u32,
        field: &Field,
    ) -> Json {
        let Some(repo) = self.repo_by_db_id(db_id) else {
            return Json::Null;
        };
        let Some(pull) = repo.pulls.iter().find(|pull| pull.number == number) else {
            return Json::Null;
        };
        let gid = self.store.pull_gid(repo, number);
        let state = if pull.merged {
            "MERGED".to_string()
        } else {
            pull.state.as_str().to_uppercase()
        };
        match field.name.as_str() {
            "id" => Json::Str(gid),
            "number" => Json::Int(number as i64),
            "title" => Json::Str(pull.title.clone()),
            "body" => Json::Str(pull.body.clone()),
            "state" => Json::Str(state),
            "headRefName" => Json::Str(pull.head_ref.clone()),
            "baseRefName" => Json::Str(pull.base_ref.clone()),
            "mergeable" => Json::Str(pull_mergeable(repo, pull).as_str().to_string()),
            "merged" => Json::Bool(pull.merged),
            "files" => {
                let first = self.arg_int(field, "first").unwrap_or(100).max(0) as usize;
                let Some(head_tip) = repo.branch_tip(&pull.head_ref) else {
                    return self.sub(
                        document,
                        Node::FileConnection(db_id, number, Vec::new(), first),
                        field,
                    );
                };
                let Some(base_commit) = repo.objects.commit_of(&pull.base_sha) else {
                    return self.sub(
                        document,
                        Node::FileConnection(db_id, number, Vec::new(), first),
                        field,
                    );
                };
                let Some(head_commit) = repo.objects.commit_of(&head_tip) else {
                    return self.sub(
                        document,
                        Node::FileConnection(db_id, number, Vec::new(), first),
                        field,
                    );
                };
                let paths: Vec<String> = diff_trees(repo, base_commit.tree, head_commit.tree)
                    .into_iter()
                    .filter(|change| change.status != "removed")
                    .map(|change| change.path)
                    .collect();
                self.sub(
                    document,
                    Node::FileConnection(db_id, number, paths, first),
                    field,
                )
            }
            _ => self.unknown_field(&Node::Pull(db_id, number), field),
        }
    }

    fn resolve_branch(&mut self, _document: &Document, name: &str, field: &Field) -> Json {
        match field.name.as_str() {
            "name" => Json::Str(name.to_string()),
            _ => self.unknown_field(&Node::Branch(name.to_string()), field),
        }
    }

    fn resolve_issue_connection(
        &mut self,
        document: &Document,
        db_id: u32,
        numbers: &[u32],
        after: Option<u32>,
        first: usize,
        field: &Field,
    ) -> Json {
        // `after` is a base64 `cursor:<number>`; the page starts after that
        // number's position in the (already ordered) list, so DESC order
        // paginates correctly too.
        let start = match after {
            Some(after) => numbers
                .iter()
                .position(|n| *n == after)
                .map(|pos| pos + 1)
                .unwrap_or(0),
            None => 0,
        };
        let mut page: Vec<u32> = numbers.iter().skip(start).copied().collect();
        let has_next = page.len() > first;
        page.truncate(first);
        let end_cursor = page
            .last()
            .map(|n| base64_encode(format!("cursor:{n}").as_bytes()));
        match field.name.as_str() {
            "nodes" => {
                let nodes: Vec<Json> = page
                    .iter()
                    .map(|number| {
                        let mut out = Vec::new();
                        self.resolve_selections(
                            document,
                            Node::Issue(db_id, *number),
                            &field.selection,
                            &mut out,
                        );
                        Json::Obj(out)
                    })
                    .collect();
                Json::Arr(nodes)
            }
            "edges" => {
                let edges: Vec<Json> = page
                    .iter()
                    .map(|number| {
                        let mut node_out = Vec::new();
                        self.resolve_selections(
                            document,
                            Node::Issue(db_id, *number),
                            &field.selection,
                            &mut node_out,
                        );
                        obj(vec![
                            (
                                "cursor",
                                Json::Str(base64_encode(format!("cursor:{number}").as_bytes())),
                            ),
                            ("node", Json::Obj(node_out)),
                        ])
                    })
                    .collect();
                Json::Arr(edges)
            }
            "pageInfo" => self.sub(document, Node::PageInfo(has_next, end_cursor), field),
            "totalCount" => Json::Int(numbers.len() as i64),
            _ => self.unknown_field(
                &Node::IssueConnection(db_id, numbers.to_vec(), after, first),
                field,
            ),
        }
    }

    fn resolve_pull_connection(
        &mut self,
        document: &Document,
        db_id: u32,
        numbers: &[u32],
        first: usize,
        field: &Field,
    ) -> Json {
        match field.name.as_str() {
            "nodes" => {
                let nodes: Vec<Json> = numbers
                    .iter()
                    .take(first)
                    .map(|number| {
                        let mut out = Vec::new();
                        self.resolve_selections(
                            document,
                            Node::Pull(db_id, *number),
                            &field.selection,
                            &mut out,
                        );
                        Json::Obj(out)
                    })
                    .collect();
                Json::Arr(nodes)
            }
            "totalCount" => Json::Int(numbers.len() as i64),
            _ => self.unknown_field(&Node::PullConnection(db_id, numbers.to_vec(), first), field),
        }
    }

    fn resolve_file_connection(
        &mut self,
        _document: &Document,
        db_id: u32,
        number: u32,
        paths: &[String],
        first: usize,
        field: &Field,
    ) -> Json {
        match field.name.as_str() {
            "nodes" => {
                let nodes: Vec<Json> = paths
                    .iter()
                    .take(first)
                    .map(|path| {
                        let mut out = Vec::new();
                        for selection in &field.selection {
                            if let Selection::Field(f) = selection {
                                if f.name == "path" {
                                    out.push((
                                        f.alias.clone().unwrap_or_else(|| "path".to_string()),
                                        Json::Str(path.clone()),
                                    ));
                                }
                            }
                        }
                        Json::Obj(out)
                    })
                    .collect();
                Json::Arr(nodes)
            }
            "totalCount" => Json::Int(paths.len() as i64),
            _ => self.unknown_field(
                &Node::FileConnection(db_id, number, paths.to_vec(), first),
                field,
            ),
        }
    }

    fn resolve_comment_edge(
        &mut self,
        document: &Document,
        id: u64,
        body: &str,
        field: &Field,
    ) -> Json {
        match field.name.as_str() {
            "node" => self.sub(document, Node::Comment(id, body.to_string()), field),
            "cursor" => Json::Str(base64_encode(format!("cursor:{id}").as_bytes())),
            _ => self.unknown_field(&Node::CommentEdge(id, body.to_string()), field),
        }
    }

    fn resolve_page_info(
        &mut self,
        field: &Field,
        has_next: bool,
        end_cursor: Option<String>,
    ) -> Json {
        match field.name.as_str() {
            "hasNextPage" => Json::Bool(has_next),
            "endCursor" => end_cursor.map(Json::Str).unwrap_or(Json::Null),
            _ => {
                self.error(format!(
                    "Cannot query field \"{}\" on type \"PageInfo\".",
                    field.name
                ));
                Json::Null
            }
        }
    }

    fn resolve_create_issue(
        &mut self,
        document: &Document,
        db_id: u32,
        number: u32,
        field: &Field,
    ) -> Json {
        match field.name.as_str() {
            "issue" => self.sub(document, Node::Issue(db_id, number), field),
            "clientMutationId" => Json::Null,
            _ => self.unknown_field(&Node::CreateIssue(db_id, number), field),
        }
    }

    fn resolve_close_issue(
        &mut self,
        document: &Document,
        db_id: u32,
        number: u32,
        field: &Field,
    ) -> Json {
        match field.name.as_str() {
            "issue" => self.sub(document, Node::Issue(db_id, number), field),
            "clientMutationId" => Json::Null,
            _ => self.unknown_field(&Node::CloseIssue(db_id, number), field),
        }
    }

    fn resolve_merge_pull(
        &mut self,
        document: &Document,
        db_id: u32,
        number: u32,
        field: &Field,
    ) -> Json {
        match field.name.as_str() {
            "pullRequest" => self.sub(document, Node::Pull(db_id, number), field),
            "clientMutationId" => Json::Null,
            _ => self.unknown_field(&Node::MergePull(db_id, number), field),
        }
    }

    fn resolve_create_pull(
        &mut self,
        document: &Document,
        db_id: u32,
        number: u32,
        field: &Field,
    ) -> Json {
        match field.name.as_str() {
            "pullRequest" => self.sub(document, Node::Pull(db_id, number), field),
            "clientMutationId" => Json::Null,
            _ => self.unknown_field(&Node::CreatePull(db_id, number), field),
        }
    }

    fn resolve_add_comment(
        &mut self,
        document: &Document,
        id: u64,
        body: &str,
        field: &Field,
    ) -> Json {
        match field.name.as_str() {
            "commentEdge" => self.sub(document, Node::CommentEdge(id, body.to_string()), field),
            "clientMutationId" => Json::Null,
            _ => self.unknown_field(&Node::AddComment(id, body.to_string()), field),
        }
    }

    fn resolve_comment(
        &mut self,
        _document: &Document,
        id: u64,
        body: &str,
        field: &Field,
    ) -> Json {
        match field.name.as_str() {
            "id" => Json::Str(base64_encode(format!("IssueComment:{id}").as_bytes())),
            "body" => Json::Str(body.to_string()),
            _ => self.unknown_field(&Node::Comment(id, body.to_string()), field),
        }
    }

    fn resolve_type(&mut self, _document: &Document, name: &str, field: &Field) -> Json {
        match field.name.as_str() {
            "name" => Json::Str(name.to_string()),
            "kind" => Json::Str(
                match name {
                    "SearchType" => "ENUM",
                    _ => "OBJECT",
                }
                .to_string(),
            ),
            "fields" => {
                let fields = type_fields(name);
                let items: Vec<Json> = fields
                    .iter()
                    .map(|field_name| {
                        let mut out = Vec::new();
                        for selection in &field.selection {
                            if let Selection::Field(f) = selection {
                                match f.name.as_str() {
                                    "name" => out.push((
                                        f.alias.clone().unwrap_or_else(|| "name".to_string()),
                                        Json::Str(field_name.to_string()),
                                    )),
                                    "type" => out.push((
                                        f.alias.clone().unwrap_or_else(|| "type".to_string()),
                                        obj(vec![("name", Json::Str(field_name.to_string()))]),
                                    )),
                                    _ => {}
                                }
                            }
                        }
                        Json::Obj(out)
                    })
                    .collect();
                Json::Arr(items)
            }
            "enumValues" => {
                let values: &[&str] = match name {
                    "SearchType" => &["ISSUE", "PULL_REQUEST", "REPOSITORY", "USER"],
                    _ => &[],
                };
                let items: Vec<Json> = values
                    .iter()
                    .map(|value| {
                        let mut out = Vec::new();
                        for selection in &field.selection {
                            if let Selection::Field(f) = selection {
                                if f.name == "name" {
                                    out.push((
                                        f.alias.clone().unwrap_or_else(|| "name".to_string()),
                                        Json::Str(value.to_string()),
                                    ));
                                }
                            }
                        }
                        Json::Obj(out)
                    })
                    .collect();
                Json::Arr(items)
            }
            _ => self.unknown_field(&Node::Type(name.to_string()), field),
        }
    }

    // ---- mutations ----

    fn resolve_mutation(&mut self, document: &Document, field: &Field) -> Json {
        let input = self.arg_value(field, "input").unwrap_or(Json::Null);
        match field.name.as_str() {
            "createIssue" => {
                let Some(repository_id) = input
                    .get("repositoryId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                else {
                    return Json::Null;
                };
                let Some(title) = input
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                else {
                    return Json::Null;
                };
                let body_text = input
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let Some((kind, db_id, _)) = self.store.decode_gid(&repository_id) else {
                    return Json::Null;
                };
                if kind != "Repository" {
                    return Json::Null;
                }
                let id = self.store.next_issue_id;
                self.store.next_issue_id += 1;
                self.store.created_seq += 1;
                let created_seq = self.store.created_seq;
                let repo = match self
                    .store
                    .repos
                    .values_mut()
                    .find(|repo| repo.db_id == db_id)
                {
                    Some(repo) => repo,
                    None => return Json::Null,
                };
                repo.number_seq += 1;
                let number = repo.number_seq;
                repo.issues.push(crate::store::Issue {
                    id,
                    number,
                    title,
                    body: body_text,
                    state: IssueState::Open,
                    labels: Vec::new(),
                    comments: Vec::new(),
                    created_seq,
                });
                self.sub(document, Node::CreateIssue(db_id, number), field)
            }
            "closeIssue" => {
                let Some(issue_id) = input
                    .get("issueId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                else {
                    return Json::Null;
                };
                let Some((kind, db_id, number)) = self.store.decode_gid(&issue_id) else {
                    return Json::Null;
                };
                if kind != "Issue" {
                    return Json::Null;
                }
                let repo = match self
                    .store
                    .repos
                    .values_mut()
                    .find(|repo| repo.db_id == db_id)
                {
                    Some(repo) => repo,
                    None => return Json::Null,
                };
                if let Some(issue) = repo.issues.iter_mut().find(|issue| issue.number == number) {
                    issue.state = IssueState::Closed;
                } else {
                    return Json::Null;
                }
                self.sub(document, Node::CloseIssue(db_id, number), field)
            }
            "createPullRequest" => {
                let Some(repository_id) = input
                    .get("repositoryId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                else {
                    return Json::Null;
                };
                let Some(base) = input
                    .get("baseRefName")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                else {
                    return Json::Null;
                };
                let Some(head) = input
                    .get("headRefName")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                else {
                    return Json::Null;
                };
                let title = input
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let body_text = input
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let Some((kind, db_id, _)) = self.store.decode_gid(&repository_id) else {
                    return Json::Null;
                };
                if kind != "Repository" {
                    return Json::Null;
                }
                let base_sha = match self
                    .store
                    .repos
                    .values()
                    .find(|repo| repo.db_id == db_id)
                    .and_then(|repo| repo.branch_tip(&base))
                {
                    Some(sha) => sha,
                    None => return Json::Null,
                };
                let id = self.store.next_pull_id;
                self.store.next_pull_id += 1;
                self.store.created_seq += 1;
                let created_seq = self.store.created_seq;
                let repo = match self
                    .store
                    .repos
                    .values_mut()
                    .find(|repo| repo.db_id == db_id)
                {
                    Some(repo) => repo,
                    None => return Json::Null,
                };
                repo.number_seq += 1;
                let number = repo.number_seq;
                repo.pulls.push(crate::store::Pull {
                    id,
                    number,
                    title,
                    body: body_text,
                    head_ref: head,
                    base_ref: base,
                    base_sha,
                    state: IssueState::Open,
                    merged: false,
                    merged_at: None,
                    merge_commit_sha: None,
                    created_seq,
                });
                self.sub(document, Node::CreatePull(db_id, number), field)
            }
            "mergePullRequest" => {
                let Some(pull_id) = input
                    .get("pullRequestId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                else {
                    return Json::Null;
                };
                let Some((kind, db_id, number)) = self.store.decode_gid(&pull_id) else {
                    return Json::Null;
                };
                if kind != "PullRequest" {
                    return Json::Null;
                }
                let identity = self.store.identity();
                let timestamp = self.store.next_commit_timestamp();
                let merged_at =
                    iso_from_epoch(crate::scenario::SCENARIO_EPOCH_SEC + self.store.commit_seq);
                let repo = match self
                    .store
                    .repos
                    .values_mut()
                    .find(|repo| repo.db_id == db_id)
                {
                    Some(repo) => repo,
                    None => return Json::Null,
                };
                match squash_merge(repo, &identity, timestamp, merged_at, number) {
                    Ok(_) => self.sub(document, Node::MergePull(db_id, number), field),
                    Err(_) => Json::Null,
                }
            }
            "addComment" => {
                let Some(subject_id) = input
                    .get("subjectId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                else {
                    return Json::Null;
                };
                let body_text = input
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let Some((kind, db_id, number)) = self.store.decode_gid(&subject_id) else {
                    return Json::Null;
                };
                if kind != "Issue" {
                    return Json::Null;
                }
                let repo = match self
                    .store
                    .repos
                    .values_mut()
                    .find(|repo| repo.db_id == db_id)
                {
                    Some(repo) => repo,
                    None => return Json::Null,
                };
                repo.comment_seq += 1;
                let comment_id = repo.comment_seq;
                let Some(issue) = repo.issues.iter_mut().find(|issue| issue.number == number)
                else {
                    return Json::Null;
                };
                issue.comments.push(crate::store::Comment {
                    id: comment_id,
                    body: body_text.clone(),
                });
                self.sub(document, Node::AddComment(comment_id, body_text), field)
            }
            _ => self.unknown_field(&Node::Mutation, field),
        }
    }
}

/// The fields the kit introspects for each type.
fn type_fields(name: &str) -> &'static [&'static str] {
    match name {
        "Issue" => &["id", "number", "title", "state", "body"],
        "PullRequest" => &[
            "id",
            "number",
            "headRefName",
            "baseRefName",
            "mergeable",
            "state",
            "merged",
            "files",
        ],
        "Repository" => &[
            "id",
            "databaseId",
            "name",
            "nameWithOwner",
            "owner",
            "defaultBranchRef",
            "viewerPermission",
            "hasIssuesEnabled",
            "issue",
            "issues",
            "pullRequests",
            "pullRequest",
        ],
        "Viewer" => &["login"],
        _ => &[],
    }
}
