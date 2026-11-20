//! GitHub GraphQL v4 (`POST /api/graphql`): the subset gh and
//! github-mcp-server actually send.
//!
//! Round 15 rewrite, written from the kit's conformance suite
//! (`sdk/conformance/checks-graphql.ts` — its 18 documents are the spec)
//! and the other modules' public interfaces. The design is my own:
//! a small lexer, a recursive-descent parser, a validation pass against
//! hardcoded field tables, and a type-directed executor over the store.
//! Global ids reuse the store's opaque `*_gid` / `decode_gid` scheme, so
//! a repository id is never the plain database id and an issue id is
//! never the plain number.
//!
//! Round 15 hardening (C1, C2): named fragments expand with a visited
//! set (a cycle errors with `fragment cycle detected: {name}`) and a
//! 10,000-expansion budget; the parser is depth-capped at 128 for both
//! selection sets (`GraphQL selection nesting too deep`) and argument
//! values (`GraphQL value nesting too deep`).
//!
//! Error policy: a GraphQL-level problem (bad document, unknown field,
//! failed mutation) is HTTP 200 with an `errors` array, as GitHub
//! answers; only a body that is not JSON at all is an HTTP 400 with
//! `{"message":"malformed JSON body"}`.

use crate::http::{Request, Response};
use crate::json::Json;
use crate::scenario::SCENARIO_EPOCH_SEC;
use crate::store::{squash_merge, Issue, IssueState, Pull, Store};
use crate::util::{base64_decode, base64_encode, iso_from_epoch};
use std::collections::HashMap;
use std::sync::RwLock;

/// The deepest selection/value nesting the parser accepts.
pub const MAX_GRAPHQL_DEPTH: usize = 128;
/// The most named-fragment expansions one operation may perform.
pub const MAX_FRAGMENT_EXPANSIONS: usize = 10_000;

/// A directive list: `(name, [(arg, value)])` per directive.
type DirectiveList = Vec<(String, Vec<(String, GqlValue)>)>;

// ---------------------------------------------------------------------------
// Lexer.

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Name(String),
    Int(i64),
    Float(f64),
    Str(String),
    Dollar,
    Bang,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Colon,
    Equals,
    At,
    Spread,
    Pipe,
    Amp,
}

fn lex(src: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            ' ' | '\t' | '\n' | '\r' | ',' => i += 1,
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '$' => {
                tokens.push(Token::Dollar);
                i += 1;
            }
            '!' => {
                tokens.push(Token::Bang);
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
            '{' => {
                tokens.push(Token::LBrace);
                i += 1;
            }
            '}' => {
                tokens.push(Token::RBrace);
                i += 1;
            }
            ':' => {
                tokens.push(Token::Colon);
                i += 1;
            }
            '=' => {
                tokens.push(Token::Equals);
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
                if i + 2 < chars.len() && chars[i + 1] == '.' && chars[i + 2] == '.' {
                    tokens.push(Token::Spread);
                    i += 3;
                } else {
                    return Err("unexpected '.' in document".to_string());
                }
            }
            '"' => {
                // Block strings (""") are read literally; anything else is
                // a normal string with backslash escapes.
                if i + 2 < chars.len() && chars[i + 1] == '"' && chars[i + 2] == '"' {
                    let mut j = i + 3;
                    let mut text = String::new();
                    while j + 2 < chars.len()
                        && !(chars[j] == '"' && chars[j + 1] == '"' && chars[j + 2] == '"')
                    {
                        text.push(chars[j]);
                        j += 1;
                    }
                    if j + 2 >= chars.len() {
                        return Err("unterminated block string".to_string());
                    }
                    tokens.push(Token::Str(text));
                    i = j + 3;
                } else {
                    let mut j = i + 1;
                    let mut text = String::new();
                    loop {
                        if j >= chars.len() {
                            return Err("unterminated string".to_string());
                        }
                        match chars[j] {
                            '"' => break,
                            '\\' => {
                                j += 1;
                                if j >= chars.len() {
                                    return Err("unterminated string".to_string());
                                }
                                match chars[j] {
                                    '"' => text.push('"'),
                                    '\\' => text.push('\\'),
                                    '/' => text.push('/'),
                                    'b' => text.push('\u{0008}'),
                                    'f' => text.push('\u{000C}'),
                                    'n' => text.push('\n'),
                                    'r' => text.push('\r'),
                                    't' => text.push('\t'),
                                    'u' => {
                                        if j + 4 >= chars.len() {
                                            return Err("bad \\u escape".to_string());
                                        }
                                        let hex: String = chars[j + 1..j + 5].iter().collect();
                                        let code = u32::from_str_radix(&hex, 16)
                                            .map_err(|_| "bad \\u escape".to_string())?;
                                        text.push(
                                            char::from_u32(code)
                                                .ok_or("bad \\u escape".to_string())?,
                                        );
                                        j += 4;
                                    }
                                    _ => {
                                        return Err("bad string escape".to_string());
                                    }
                                }
                            }
                            c => text.push(c),
                        }
                        j += 1;
                    }
                    tokens.push(Token::Str(text));
                    i = j + 1;
                }
            }
            '-' | '0'..='9' => {
                let start = i;
                if chars[i] == '-' {
                    i += 1;
                }
                let mut is_float = false;
                while i < chars.len()
                    && (chars[i].is_ascii_digit()
                        || chars[i] == '.'
                        || chars[i] == 'e'
                        || chars[i] == 'E'
                        || chars[i] == '+'
                        || chars[i] == '-')
                {
                    if chars[i] == '.' || chars[i] == 'e' || chars[i] == 'E' {
                        is_float = true;
                    }
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                if is_float {
                    tokens.push(Token::Float(
                        text.parse::<f64>().map_err(|_| "bad number".to_string())?,
                    ));
                } else {
                    tokens.push(Token::Int(
                        text.parse::<i64>().map_err(|_| "bad number".to_string())?,
                    ));
                }
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                tokens.push(Token::Name(chars[start..i].iter().collect()));
            }
            _ => return Err(format!("unexpected character '{ch}' in document")),
        }
    }
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// AST.

/// An argument value as written in the document.
#[derive(Clone, Debug)]
enum GqlValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// A bare name (`OPEN`, `CREATED_AT`): an enum literal.
    Enum(String),
    List(Vec<GqlValue>),
    Object(Vec<(String, GqlValue)>),
    Var(String),
}

impl GqlValue {
    /// Resolve variables against the request's variables map. Enum literals
    /// become plain strings; everything else mirrors the JSON shape.
    fn resolve(&self, vars: &HashMap<String, Json>) -> Json {
        match self {
            GqlValue::Null => Json::Null,
            GqlValue::Bool(flag) => Json::Bool(*flag),
            GqlValue::Int(n) => Json::Int(*n),
            GqlValue::Float(f) => Json::Float(*f),
            GqlValue::Str(text) => Json::Str(text.clone()),
            GqlValue::Enum(name) => Json::Str(name.clone()),
            GqlValue::List(items) => {
                Json::Arr(items.iter().map(|item| item.resolve(vars)).collect())
            }
            GqlValue::Object(fields) => Json::Obj(
                fields
                    .iter()
                    .map(|(name, value)| (name.clone(), value.resolve(vars)))
                    .collect(),
            ),
            GqlValue::Var(name) => vars.get(name).cloned().unwrap_or(Json::Null),
        }
    }
}

/// One field selection: `alias: name(args) @directives { sub }`.
#[derive(Clone, Debug)]
struct FieldSel {
    alias: Option<String>,
    name: String,
    args: Vec<(String, GqlValue)>,
    directives: DirectiveList,
    sub: Vec<Selection>,
}

#[derive(Clone, Debug)]
enum Selection {
    Field(FieldSel),
    /// `...Name @directives`
    Spread(String, DirectiveList),
    /// `... [on Type] @directives { sub }`
    Inline(Option<String>, DirectiveList, Vec<Selection>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OpKind {
    Query,
    Mutation,
}

#[derive(Clone, Debug)]
struct Operation {
    kind: OpKind,
    name: Option<String>,
    /// Variable definitions: name and optional default value.
    var_defs: Vec<(String, Option<GqlValue>)>,
    selections: Vec<Selection>,
}

#[derive(Clone, Debug)]
struct FragmentDef {
    type_cond: String,
    selections: Vec<Selection>,
}

#[derive(Default)]
struct Document {
    ops: Vec<Operation>,
    frags: HashMap<String, FragmentDef>,
}

// ---------------------------------------------------------------------------
// Parser (recursive descent; depth-capped).

struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let tok = self.toks.get(self.pos).cloned();
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    fn expect(&mut self, want: &Token) -> Result<(), String> {
        match self.next() {
            Some(tok) if &tok == want => Ok(()),
            Some(tok) => Err(format!("expected {want:?}, found {tok:?}")),
            None => Err(format!("expected {want:?}, found end of document")),
        }
    }

    fn expect_name(&mut self) -> Result<String, String> {
        match self.next() {
            Some(Token::Name(name)) => Ok(name),
            Some(tok) => Err(format!("expected a name, found {tok:?}")),
            None => Err("expected a name, found end of document".to_string()),
        }
    }

    fn at_punct(&self, want: &Token) -> bool {
        self.peek() == Some(want)
    }

    fn parse_document(&mut self) -> Result<Document, String> {
        let mut doc = Document::default();
        while self.peek().is_some() {
            match self.peek() {
                Some(Token::LBrace) => {
                    let selections = self.parse_selection_set(0)?;
                    doc.ops.push(Operation {
                        kind: OpKind::Query,
                        name: None,
                        var_defs: Vec::new(),
                        selections,
                    });
                }
                Some(Token::Name(keyword)) if keyword == "query" || keyword == "mutation" => {
                    let kind = if keyword == "query" {
                        OpKind::Query
                    } else {
                        OpKind::Mutation
                    };
                    self.next();
                    let name = match self.peek() {
                        Some(Token::Name(_)) => Some(self.expect_name()?),
                        _ => None,
                    };
                    let var_defs = if self.at_punct(&Token::LParen) {
                        self.parse_var_defs()?
                    } else {
                        Vec::new()
                    };
                    // Directives on the operation itself are parsed and ignored.
                    while self.at_punct(&Token::At) {
                        self.parse_directive()?;
                    }
                    let selections = self.parse_selection_set(0)?;
                    doc.ops.push(Operation {
                        kind,
                        name,
                        var_defs,
                        selections,
                    });
                }
                Some(Token::Name(keyword)) if keyword == "fragment" => {
                    self.next();
                    let name = self.expect_name()?;
                    let on = self.expect_name()?;
                    if on != "on" {
                        return Err(format!("expected 'on' in fragment, found {on}"));
                    }
                    let type_cond = self.expect_name()?;
                    while self.at_punct(&Token::At) {
                        self.parse_directive()?;
                    }
                    let selections = self.parse_selection_set(0)?;
                    doc.frags.insert(
                        name.clone(),
                        FragmentDef {
                            type_cond,
                            selections,
                        },
                    );
                }
                Some(tok) => {
                    return Err(format!("unexpected {tok:?} at the top of a document"));
                }
                None => break,
            }
        }
        if doc.ops.is_empty() {
            return Err("document has no operation".to_string());
        }
        Ok(doc)
    }

    fn parse_var_defs(&mut self) -> Result<Vec<(String, Option<GqlValue>)>, String> {
        self.expect(&Token::LParen)?;
        let mut defs = Vec::new();
        while !self.at_punct(&Token::RParen) {
            self.expect(&Token::Dollar)?;
            let name = self.expect_name()?;
            self.expect(&Token::Colon)?;
            self.parse_type()?;
            let default = if self.at_punct(&Token::Equals) {
                self.next();
                Some(self.parse_value(0)?)
            } else {
                None
            };
            defs.push((name, default));
            if self.peek().is_none() {
                return Err("unterminated variable definitions".to_string());
            }
        }
        self.expect(&Token::RParen)?;
        Ok(defs)
    }

    /// A type expression (`String!`, `[Int]`, `CreateIssueInput!`): parsed
    /// as an opaque shape — execution only needs the variable values.
    fn parse_type(&mut self) -> Result<(), String> {
        match self.next() {
            Some(Token::LBracket) => {
                self.parse_type()?;
                self.expect(&Token::RBracket)?;
            }
            Some(Token::Name(_)) => {}
            Some(tok) => return Err(format!("expected a type, found {tok:?}")),
            None => return Err("expected a type, found end of document".to_string()),
        }
        if self.at_punct(&Token::Bang) {
            self.next();
        }
        Ok(())
    }

    fn parse_selection_set(&mut self, depth: usize) -> Result<Vec<Selection>, String> {
        if depth > MAX_GRAPHQL_DEPTH {
            return Err("GraphQL selection nesting too deep".to_string());
        }
        self.expect(&Token::LBrace)?;
        let mut selections = Vec::new();
        while !self.at_punct(&Token::RBrace) {
            if self.peek().is_none() {
                return Err("unterminated selection set".to_string());
            }
            selections.push(self.parse_selection(depth)?);
        }
        self.expect(&Token::RBrace)?;
        Ok(selections)
    }

    fn parse_selection(&mut self, depth: usize) -> Result<Selection, String> {
        if self.at_punct(&Token::Spread) {
            self.next();
            match self.peek() {
                Some(Token::Name(name)) if name == "on" => {
                    self.next();
                    let type_cond = self.expect_name()?;
                    let directives = self.parse_directives()?;
                    let sub = self.parse_selection_set(depth + 1)?;
                    Ok(Selection::Inline(Some(type_cond), directives, sub))
                }
                Some(Token::Name(_)) => {
                    let name = self.expect_name()?;
                    let directives = self.parse_directives()?;
                    Ok(Selection::Spread(name, directives))
                }
                Some(Token::LBrace) => {
                    let directives = self.parse_directives()?;
                    let sub = self.parse_selection_set(depth + 1)?;
                    Ok(Selection::Inline(None, directives, sub))
                }
                _ => {
                    let directives = self.parse_directives()?;
                    let sub = self.parse_selection_set(depth + 1)?;
                    Ok(Selection::Inline(None, directives, sub))
                }
            }
        } else {
            let first = self.expect_name()?;
            let (alias, name) = if self.at_punct(&Token::Colon) {
                self.next();
                (Some(first), self.expect_name()?)
            } else {
                (None, first)
            };
            let args = if self.at_punct(&Token::LParen) {
                self.parse_args()?
            } else {
                Vec::new()
            };
            let directives = self.parse_directives()?;
            let sub = if self.at_punct(&Token::LBrace) {
                self.parse_selection_set(depth + 1)?
            } else {
                Vec::new()
            };
            Ok(Selection::Field(FieldSel {
                alias,
                name,
                args,
                directives,
                sub,
            }))
        }
    }

    fn parse_directives(&mut self) -> Result<DirectiveList, String> {
        let mut directives = Vec::new();
        while self.at_punct(&Token::At) {
            directives.push(self.parse_directive()?);
        }
        Ok(directives)
    }

    fn parse_directive(&mut self) -> Result<(String, Vec<(String, GqlValue)>), String> {
        self.expect(&Token::At)?;
        let name = self.expect_name()?;
        let args = if self.at_punct(&Token::LParen) {
            self.parse_args()?
        } else {
            Vec::new()
        };
        Ok((name, args))
    }

    fn parse_args(&mut self) -> Result<Vec<(String, GqlValue)>, String> {
        self.expect(&Token::LParen)?;
        let mut args = Vec::new();
        while !self.at_punct(&Token::RParen) {
            if self.peek().is_none() {
                return Err("unterminated arguments".to_string());
            }
            let name = self.expect_name()?;
            self.expect(&Token::Colon)?;
            let value = self.parse_value(0)?;
            args.push((name, value));
        }
        self.expect(&Token::RParen)?;
        Ok(args)
    }

    fn parse_value(&mut self, depth: usize) -> Result<GqlValue, String> {
        if depth > MAX_GRAPHQL_DEPTH {
            return Err("GraphQL value nesting too deep".to_string());
        }
        match self.next() {
            Some(Token::Dollar) => Ok(GqlValue::Var(self.expect_name()?)),
            Some(Token::Int(n)) => Ok(GqlValue::Int(n)),
            Some(Token::Float(f)) => Ok(GqlValue::Float(f)),
            Some(Token::Str(text)) => Ok(GqlValue::Str(text)),
            Some(Token::Name(name)) => match name.as_str() {
                "true" => Ok(GqlValue::Bool(true)),
                "false" => Ok(GqlValue::Bool(false)),
                "null" => Ok(GqlValue::Null),
                _ => Ok(GqlValue::Enum(name)),
            },
            Some(Token::LBracket) => {
                let mut items = Vec::new();
                while !self.at_punct(&Token::RBracket) {
                    if self.peek().is_none() {
                        return Err("unterminated list value".to_string());
                    }
                    items.push(self.parse_value(depth + 1)?);
                }
                self.expect(&Token::RBracket)?;
                Ok(GqlValue::List(items))
            }
            Some(Token::LBrace) => {
                let mut fields = Vec::new();
                while !self.at_punct(&Token::RBrace) {
                    if self.peek().is_none() {
                        return Err("unterminated object value".to_string());
                    }
                    let name = self.expect_name()?;
                    self.expect(&Token::Colon)?;
                    let value = self.parse_value(depth + 1)?;
                    fields.push((name, value));
                }
                self.expect(&Token::RBrace)?;
                Ok(GqlValue::Object(fields))
            }
            Some(tok) => Err(format!("unexpected {tok:?} as a value")),
            None => Err("unexpected end of document in a value".to_string()),
        }
    }
}

fn parse_document(src: &str) -> Result<Document, String> {
    let toks = lex(src)?;
    let mut parser = Parser { toks, pos: 0 };
    parser.parse_document()
}

// ---------------------------------------------------------------------------
// Schema tables (the surface the suite's documents use).

/// Field name -> result type for every object type the executor knows.
/// List fields name their element type; scalar-ness decides whether a
/// sub-selection is legal.
fn fields_of(type_name: &str) -> Option<&'static [(&'static str, &'static str)]> {
    Some(match type_name {
        "Query" => &[
            ("viewer", "User"),
            ("repository", "Repository"),
            ("node", "Node"),
            ("__type", "__Type"),
        ],
        "Mutation" => &[
            ("createIssue", "CreateIssuePayload"),
            ("closeIssue", "CloseIssuePayload"),
            ("createPullRequest", "CreatePullRequestPayload"),
            ("mergePullRequest", "MergePullRequestPayload"),
            ("addComment", "AddCommentPayload"),
        ],
        "User" => &[
            ("login", "String"),
            ("id", "ID"),
            ("name", "String"),
            ("email", "String"),
        ],
        "Repository" => &[
            ("id", "ID"),
            ("databaseId", "Int"),
            ("name", "String"),
            ("nameWithOwner", "String"),
            ("owner", "User"),
            ("defaultBranchRef", "Ref"),
            ("viewerPermission", "RepositoryPermission"),
            ("hasIssuesEnabled", "Boolean"),
            ("issue", "Issue"),
            ("issues", "IssueConnection"),
            ("pullRequests", "PullRequestConnection"),
            ("pullRequest", "PullRequest"),
            ("description", "String"),
            ("url", "String"),
            ("isPrivate", "Boolean"),
        ],
        "Issue" => &[
            ("id", "ID"),
            ("number", "Int"),
            ("title", "String"),
            ("state", "IssueState"),
            ("body", "String"),
            ("url", "String"),
            ("createdAt", "String"),
            ("author", "User"),
            ("labels", "Label"),
        ],
        "PullRequest" => &[
            ("id", "ID"),
            ("number", "Int"),
            ("title", "String"),
            ("body", "String"),
            ("state", "PullRequestState"),
            ("merged", "Boolean"),
            ("headRefName", "String"),
            ("baseRefName", "String"),
            ("mergeable", "MergeableState"),
            ("files", "PullRequestChangedFileConnection"),
            ("url", "String"),
            ("createdAt", "String"),
            ("author", "User"),
        ],
        "Ref" => &[("name", "String")],
        "IssueConnection" => &[
            ("nodes", "Issue"),
            ("edges", "IssueEdge"),
            ("pageInfo", "PageInfo"),
            ("totalCount", "Int"),
        ],
        "IssueEdge" => &[("node", "Issue"), ("cursor", "String")],
        "PageInfo" => &[
            ("hasNextPage", "Boolean"),
            ("hasPreviousPage", "Boolean"),
            ("startCursor", "String"),
            ("endCursor", "String"),
        ],
        "PullRequestConnection" => &[
            ("nodes", "PullRequest"),
            ("edges", "PullRequestEdge"),
            ("pageInfo", "PageInfo"),
            ("totalCount", "Int"),
        ],
        "PullRequestEdge" => &[("node", "PullRequest"), ("cursor", "String")],
        "PullRequestChangedFileConnection" => &[
            ("nodes", "PullRequestChangedFile"),
            ("pageInfo", "PageInfo"),
            ("totalCount", "Int"),
        ],
        "PullRequestChangedFile" => &[
            ("path", "String"),
            ("additions", "Int"),
            ("deletions", "Int"),
            ("changeType", "String"),
        ],
        "__Type" => &[
            ("name", "String"),
            ("kind", "String"),
            ("description", "String"),
            ("fields", "__Field"),
            ("enumValues", "__EnumValue"),
        ],
        "__Field" => &[("name", "String"), ("description", "String")],
        "__EnumValue" => &[("name", "String"), ("description", "String")],
        "Label" => &[
            ("name", "String"),
            ("color", "String"),
            ("description", "String"),
        ],
        "IssueComment" => &[("id", "ID"), ("body", "String"), ("author", "User")],
        "CommentEdge" => &[("node", "IssueComment")],
        "CreateIssuePayload" => &[("issue", "Issue")],
        "CloseIssuePayload" => &[("issue", "Issue")],
        "CreatePullRequestPayload" => &[("pullRequest", "PullRequest")],
        "MergePullRequestPayload" => &[("pullRequest", "PullRequest")],
        "AddCommentPayload" => &[("commentEdge", "CommentEdge")],
        "Node" => &[("id", "ID")],
        _ => return None,
    })
}

fn is_scalar(type_name: &str) -> bool {
    matches!(
        type_name,
        "String"
            | "Int"
            | "Float"
            | "Boolean"
            | "ID"
            | "IssueState"
            | "PullRequestState"
            | "MergeableState"
            | "RepositoryPermission"
            | "IssueOrderField"
            | "OrderDirection"
            | "PullRequestMergeMethod"
            | "SearchType"
    )
}

fn is_enum(type_name: &str) -> bool {
    matches!(
        type_name,
        "IssueState"
            | "PullRequestState"
            | "MergeableState"
            | "RepositoryPermission"
            | "IssueOrderField"
            | "OrderDirection"
            | "PullRequestMergeMethod"
            | "SearchType"
    )
}

fn enum_values(type_name: &str) -> &'static [&'static str] {
    match type_name {
        "SearchType" => &[
            "ISSUE",
            "PULL_REQUEST",
            "REPOSITORY",
            "USER",
            "DISCUSSION",
            "COMMIT",
            "MILESTONE",
        ],
        "IssueState" => &["OPEN", "CLOSED"],
        "PullRequestState" => &["OPEN", "CLOSED", "MERGED"],
        "MergeableState" => &["MERGEABLE", "CONFLICTING", "UNKNOWN"],
        "RepositoryPermission" => &["ADMIN", "MAINTAIN", "WRITE", "TRIAGE", "READ"],
        "IssueOrderField" => &["CREATED_AT", "UPDATED_AT", "COMMENTS"],
        "OrderDirection" => &["ASC", "DESC"],
        "PullRequestMergeMethod" => &["MERGE", "SQUASH", "REBASE"],
        _ => &[],
    }
}

fn type_kind(type_name: &str) -> &'static str {
    if is_enum(type_name) {
        "ENUM"
    } else if is_scalar(type_name) {
        "SCALAR"
    } else if type_name == "Node" {
        "INTERFACE"
    } else {
        "OBJECT"
    }
}

// ---------------------------------------------------------------------------
// Validation: unknown fields, missing/extra sub-selections, fragment
// cycles and the expansion budget. A document that fails validation is
// answered with data null and the errors, never executed.

fn validate_operation(doc: &Document, op: &Operation) -> Vec<String> {
    let mut errors = Vec::new();
    let root = match op.kind {
        OpKind::Query => "Query",
        OpKind::Mutation => "Mutation",
    };
    let mut stack: Vec<String> = Vec::new();
    let mut budget = 0usize;
    validate_selections(
        root,
        &op.selections,
        doc,
        &mut stack,
        &mut budget,
        &mut errors,
    );
    errors
}

fn validate_selections(
    type_name: &str,
    selections: &[Selection],
    doc: &Document,
    stack: &mut Vec<String>,
    budget: &mut usize,
    errors: &mut Vec<String>,
) {
    for selection in selections {
        match selection {
            Selection::Field(field) => {
                if field.name == "__typename" {
                    if !field.sub.is_empty() {
                        errors.push(
                            "Field \"__typename\" must not have a selection since type \"String\" has no subfields."
                                .to_string(),
                        );
                    }
                    continue;
                }
                let Some(fields) = fields_of(type_name) else {
                    errors.push(format!("Unknown type \"{type_name}\"."));
                    continue;
                };
                let Some((_, result)) = fields.iter().find(|(name, _)| *name == field.name) else {
                    errors.push(format!(
                        "Cannot query field \"{}\" on type \"{type_name}\".",
                        field.name
                    ));
                    continue;
                };
                if is_scalar(result) {
                    if !field.sub.is_empty() {
                        errors.push(format!(
                            "Field \"{}\" must not have a selection since type \"{result}\" has no subfields.",
                            field.name
                        ));
                    }
                } else if field.sub.is_empty() {
                    errors.push(format!(
                        "Field \"{}\" of type \"{result}\" must have a selection of subfields.",
                        field.name
                    ));
                } else {
                    validate_selections(result, &field.sub, doc, stack, budget, errors);
                }
            }
            Selection::Spread(name, _) => {
                if stack.contains(name) {
                    errors.push(format!("fragment cycle detected: {name}"));
                    continue;
                }
                if *budget >= MAX_FRAGMENT_EXPANSIONS {
                    errors.push("fragment expansion limit exceeded".to_string());
                    continue;
                }
                let Some(fragment) = doc.frags.get(name) else {
                    errors.push(format!("Unknown fragment \"{name}\"."));
                    continue;
                };
                if fragment.type_cond != type_name
                    && type_name != "Node"
                    && fragment.type_cond != "Node"
                {
                    errors.push(format!(
                        "Fragment \"{name}\" cannot be spread here as objects of type \"{type_name}\" can never be of type \"{}\".",
                        fragment.type_cond
                    ));
                    continue;
                }
                *budget += 1;
                stack.push(name.clone());
                let cond = fragment.type_cond.clone();
                validate_selections(&cond, &fragment.selections, doc, stack, budget, errors);
                stack.pop();
            }
            Selection::Inline(cond, _, sub) => {
                let target = cond.as_deref().unwrap_or(type_name);
                if fields_of(target).is_none() {
                    errors.push(format!("Unknown type \"{target}\"."));
                    continue;
                }
                validate_selections(target, sub, doc, stack, budget, errors);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Resolved values and the object model.

enum Resolved {
    Null,
    Value(Json),
    Object(GqlObj),
    Objects(Vec<GqlObj>),
}

#[derive(Clone, Debug)]
enum GqlObj {
    RootQuery,
    RootMutation,
    Viewer,
    Repo {
        owner: String,
        name: String,
    },
    Issue {
        owner: String,
        name: String,
        number: u32,
    },
    Pull {
        owner: String,
        name: String,
        number: u32,
    },
    BranchRef {
        branch: String,
    },
    IssueConn {
        owner: String,
        name: String,
        numbers: Vec<u32>,
        has_next: bool,
        end_cursor: Option<String>,
    },
    IssueEdge {
        owner: String,
        name: String,
        number: u32,
        cursor: String,
    },
    PullConn {
        owner: String,
        name: String,
        numbers: Vec<u32>,
    },
    PullEdge {
        owner: String,
        name: String,
        number: u32,
        cursor: String,
    },
    PageInfo {
        has_next: bool,
        has_prev: bool,
        start_cursor: Option<String>,
        end_cursor: Option<String>,
    },
    FileConn {
        files: Vec<crate::store::FileChange>,
    },
    ChangedFile {
        path: String,
        additions: u32,
        deletions: u32,
        status: String,
    },
    TypeInfo {
        name: String,
    },
    FieldInfo {
        name: String,
    },
    EnumValueInfo {
        name: String,
    },
    Label {
        name: String,
        color: String,
    },
    Comment {
        id: String,
        body: String,
    },
    CommentEdge {
        id: String,
        body: String,
    },
    IssuePayload {
        owner: String,
        name: String,
        number: u32,
    },
    PullPayload {
        owner: String,
        name: String,
        number: u32,
    },
    CommentPayload {
        id: String,
        body: String,
    },
}

impl GqlObj {
    fn type_name(&self) -> &'static str {
        match self {
            GqlObj::RootQuery => "Query",
            GqlObj::RootMutation => "Mutation",
            GqlObj::Viewer => "User",
            GqlObj::Repo { .. } => "Repository",
            GqlObj::Issue { .. } => "Issue",
            GqlObj::Pull { .. } => "PullRequest",
            GqlObj::BranchRef { .. } => "Ref",
            GqlObj::IssueConn { .. } => "IssueConnection",
            GqlObj::IssueEdge { .. } => "IssueEdge",
            GqlObj::PullConn { .. } => "PullRequestConnection",
            GqlObj::PullEdge { .. } => "PullRequestEdge",
            GqlObj::PageInfo { .. } => "PageInfo",
            GqlObj::FileConn { .. } => "PullRequestChangedFileConnection",
            GqlObj::ChangedFile { .. } => "PullRequestChangedFile",
            GqlObj::TypeInfo { .. } => "__Type",
            GqlObj::FieldInfo { .. } => "__Field",
            GqlObj::EnumValueInfo { .. } => "__EnumValue",
            GqlObj::Label { .. } => "Label",
            GqlObj::Comment { .. } => "IssueComment",
            GqlObj::CommentEdge { .. } => "CommentEdge",
            GqlObj::IssuePayload { .. } => "CreateIssuePayload",
            GqlObj::PullPayload { .. } => "CreatePullRequestPayload",
            GqlObj::CommentPayload { .. } => "AddCommentPayload",
        }
    }
}

/// A page cursor: base64 of `cursor:{issue number}`.
fn cursor_for(number: u32) -> String {
    base64_encode(format!("cursor:{number}").as_bytes())
}

fn cursor_number(cursor: &str) -> Option<u32> {
    let bytes = base64_decode(cursor)?;
    let text = std::str::from_utf8(&bytes).ok()?;
    text.strip_prefix("cursor:")?.parse().ok()
}

fn arg_str<'a>(args: &'a [(String, Json)], name: &str) -> Option<&'a str> {
    args.iter()
        .find(|(key, _)| key == name)
        .and_then(|(_, value)| value.as_str())
}

fn arg_int(args: &[(String, Json)], name: &str) -> Option<i64> {
    args.iter()
        .find(|(key, _)| key == name)
        .and_then(|(_, value)| value.as_i64())
}

fn arg_obj<'a>(args: &'a [(String, Json)], name: &str) -> Option<&'a Json> {
    args.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value)
}

fn obj_field<'a>(value: &'a Json, name: &str) -> Option<&'a Json> {
    value.get(name)
}

/// The repository key for a database id, if any repository carries it.
fn repo_key_by_db_id(store: &Store, db_id: u32) -> Option<(String, String)> {
    store
        .repos
        .iter()
        .find(|(_, repo)| repo.db_id == db_id)
        .map(|(key, _)| key.clone())
}

// ---------------------------------------------------------------------------
// Executor.

struct Exec<'a> {
    store: &'a mut Store,
    doc: &'a Document,
    vars: HashMap<String, Json>,
    errors: Vec<String>,
    frag_stack: Vec<String>,
    frag_budget: usize,
}

impl<'a> Exec<'a> {
    fn run(&mut self, op: &Operation) -> Json {
        let root = match op.kind {
            OpKind::Query => GqlObj::RootQuery,
            OpKind::Mutation => GqlObj::RootMutation,
        };
        let mut out: Vec<(String, Json)> = Vec::new();
        self.exec_selections(&root, &op.selections, &mut out);
        Json::Obj(out)
    }

    fn exec_selections(
        &mut self,
        obj: &GqlObj,
        selections: &[Selection],
        out: &mut Vec<(String, Json)>,
    ) {
        for selection in selections {
            match selection {
                Selection::Field(field) => {
                    if self.directive_skips(&field.directives) {
                        continue;
                    }
                    let key = field.alias.clone().unwrap_or_else(|| field.name.clone());
                    let value = self.exec_field(obj, field);
                    out.push((key, value));
                }
                Selection::Spread(name, directives) => {
                    if self.directive_skips(directives) {
                        continue;
                    }
                    if self.frag_stack.contains(name) {
                        self.errors.push(format!("fragment cycle detected: {name}"));
                        continue;
                    }
                    if self.frag_budget >= MAX_FRAGMENT_EXPANSIONS {
                        self.errors
                            .push("fragment expansion limit exceeded".to_string());
                        continue;
                    }
                    let Some(fragment) = self.doc.frags.get(name) else {
                        self.errors.push(format!("Unknown fragment \"{name}\"."));
                        continue;
                    };
                    self.frag_budget += 1;
                    self.frag_stack.push(name.clone());
                    let sub = fragment.selections.clone();
                    self.exec_selections(obj, &sub, out);
                    self.frag_stack.pop();
                }
                Selection::Inline(cond, directives, sub) => {
                    if self.directive_skips(directives) {
                        continue;
                    }
                    let applies = match cond {
                        None => true,
                        Some(target) => target == obj.type_name() || target == "Node",
                    };
                    if applies {
                        self.exec_selections(obj, sub, out);
                    }
                }
            }
        }
    }

    /// `@skip(if: true)` or `@include(if: false)` omits the selection.
    fn directive_skips(&self, directives: &DirectiveList) -> bool {
        for (name, args) in directives {
            let cond = args
                .iter()
                .find(|(key, _)| key == "if")
                .map(|(_, value)| value.resolve(&self.vars))
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            if name == "skip" && cond {
                return true;
            }
            if name == "include" && !cond {
                return true;
            }
        }
        false
    }

    fn exec_field(&mut self, obj: &GqlObj, field: &FieldSel) -> Json {
        let args: Vec<(String, Json)> = field
            .args
            .iter()
            .map(|(name, value)| (name.clone(), value.resolve(&self.vars)))
            .collect();
        match self.resolve(obj, &field.name, &args) {
            Ok(Resolved::Null) => Json::Null,
            Ok(Resolved::Value(value)) => value,
            Ok(Resolved::Object(inner)) => {
                let mut out = Vec::new();
                self.exec_selections(&inner, &field.sub, &mut out);
                Json::Obj(out)
            }
            Ok(Resolved::Objects(list)) => {
                let mut arr = Vec::new();
                for inner in list {
                    let mut out = Vec::new();
                    self.exec_selections(&inner, &field.sub, &mut out);
                    arr.push(Json::Obj(out));
                }
                Json::Arr(arr)
            }
            Err(message) => {
                self.errors.push(message);
                Json::Null
            }
        }
    }

    // -- field resolution -------------------------------------------------

    fn resolve(
        &mut self,
        obj: &GqlObj,
        field: &str,
        args: &[(String, Json)],
    ) -> Result<Resolved, String> {
        if field == "__typename" {
            return Ok(Resolved::Value(Json::Str(obj.type_name().to_string())));
        }
        match obj {
            GqlObj::RootQuery => self.resolve_query(field, args),
            GqlObj::RootMutation => self.resolve_mutation(field, args),
            GqlObj::Viewer => self.resolve_user(field),
            GqlObj::Repo { owner, name } => self.resolve_repo(owner, name, field, args),
            GqlObj::Issue {
                owner,
                name,
                number,
            } => self.resolve_issue(owner, name, *number, field),
            GqlObj::Pull {
                owner,
                name,
                number,
            } => self.resolve_pull(owner, name, *number, field, args),
            GqlObj::BranchRef { branch } => match field {
                "name" => Ok(Resolved::Value(Json::Str(branch.clone()))),
                _ => Err(format!("Cannot query field \"{field}\" on type \"Ref\".")),
            },
            GqlObj::IssueConn {
                owner,
                name,
                numbers,
                has_next,
                end_cursor,
            } => {
                self.resolve_issue_conn(owner, name, numbers, *has_next, end_cursor.clone(), field)
            }
            GqlObj::IssueEdge {
                owner,
                name,
                number,
                cursor,
            } => match field {
                "node" => Ok(Resolved::Object(GqlObj::Issue {
                    owner: owner.clone(),
                    name: name.clone(),
                    number: *number,
                })),
                "cursor" => Ok(Resolved::Value(Json::Str(cursor.clone()))),
                _ => Err(format!(
                    "Cannot query field \"{field}\" on type \"IssueEdge\"."
                )),
            },
            GqlObj::PullConn {
                owner,
                name,
                numbers,
            } => self.resolve_pull_conn(owner, name, numbers, field),
            GqlObj::PullEdge {
                owner,
                name,
                number,
                cursor,
            } => match field {
                "node" => Ok(Resolved::Object(GqlObj::Pull {
                    owner: owner.clone(),
                    name: name.clone(),
                    number: *number,
                })),
                "cursor" => Ok(Resolved::Value(Json::Str(cursor.clone()))),
                _ => Err(format!(
                    "Cannot query field \"{field}\" on type \"PullRequestEdge\"."
                )),
            },
            GqlObj::PageInfo {
                has_next,
                has_prev,
                start_cursor,
                end_cursor,
            } => match field {
                "hasNextPage" => Ok(Resolved::Value(Json::Bool(*has_next))),
                "hasPreviousPage" => Ok(Resolved::Value(Json::Bool(*has_prev))),
                "startCursor" => Ok(Resolved::Value(
                    start_cursor.clone().map(Json::Str).unwrap_or(Json::Null),
                )),
                "endCursor" => Ok(Resolved::Value(
                    end_cursor.clone().map(Json::Str).unwrap_or(Json::Null),
                )),
                _ => Err(format!(
                    "Cannot query field \"{field}\" on type \"PageInfo\"."
                )),
            },
            GqlObj::FileConn { files } => match field {
                "nodes" => Ok(Resolved::Objects(
                    files
                        .iter()
                        .map(|change| GqlObj::ChangedFile {
                            path: change.path.clone(),
                            additions: change.additions,
                            deletions: change.deletions,
                            status: change.status.to_string(),
                        })
                        .collect(),
                )),
                "pageInfo" => Ok(Resolved::Object(GqlObj::PageInfo {
                    has_next: false,
                    has_prev: false,
                    start_cursor: None,
                    end_cursor: None,
                })),
                "totalCount" => Ok(Resolved::Value(Json::Int(files.len() as i64))),
                _ => Err(format!(
                    "Cannot query field \"{field}\" on type \"PullRequestChangedFileConnection\"."
                )),
            },
            GqlObj::ChangedFile {
                path,
                additions,
                deletions,
                status,
            } => match field {
                "path" => Ok(Resolved::Value(Json::Str(path.clone()))),
                "additions" => Ok(Resolved::Value(Json::Int(*additions as i64))),
                "deletions" => Ok(Resolved::Value(Json::Int(*deletions as i64))),
                "changeType" => Ok(Resolved::Value(Json::Str(
                    match status.as_str() {
                        "added" => "ADDED",
                        "removed" => "REMOVED",
                        _ => "MODIFIED",
                    }
                    .to_string(),
                ))),
                _ => Err(format!(
                    "Cannot query field \"{field}\" on type \"PullRequestChangedFile\"."
                )),
            },
            GqlObj::TypeInfo { name } => self.resolve_type_info(name, field),
            GqlObj::FieldInfo { name } => match field {
                "name" => Ok(Resolved::Value(Json::Str(name.clone()))),
                "description" => Ok(Resolved::Null),
                _ => Err(format!(
                    "Cannot query field \"{field}\" on type \"__Field\"."
                )),
            },
            GqlObj::EnumValueInfo { name } => match field {
                "name" => Ok(Resolved::Value(Json::Str(name.clone()))),
                "description" => Ok(Resolved::Null),
                _ => Err(format!(
                    "Cannot query field \"{field}\" on type \"__EnumValue\"."
                )),
            },
            GqlObj::Label { name, color } => match field {
                "name" => Ok(Resolved::Value(Json::Str(name.clone()))),
                "color" => Ok(Resolved::Value(Json::Str(color.clone()))),
                "description" => Ok(Resolved::Value(Json::Str(String::new()))),
                _ => Err(format!("Cannot query field \"{field}\" on type \"Label\".")),
            },
            GqlObj::Comment { id, body } => match field {
                "id" => Ok(Resolved::Value(Json::Str(id.clone()))),
                "body" => Ok(Resolved::Value(Json::Str(body.clone()))),
                "author" => Ok(Resolved::Object(GqlObj::Viewer)),
                _ => Err(format!(
                    "Cannot query field \"{field}\" on type \"IssueComment\"."
                )),
            },
            GqlObj::CommentEdge { id, body } => match field {
                "node" => Ok(Resolved::Object(GqlObj::Comment {
                    id: id.clone(),
                    body: body.clone(),
                })),
                _ => Err(format!(
                    "Cannot query field \"{field}\" on type \"CommentEdge\"."
                )),
            },
            GqlObj::IssuePayload {
                owner,
                name,
                number,
            } => match field {
                "issue" => Ok(Resolved::Object(GqlObj::Issue {
                    owner: owner.clone(),
                    name: name.clone(),
                    number: *number,
                })),
                _ => Err(format!(
                    "Cannot query field \"{field}\" on type \"CreateIssuePayload\"."
                )),
            },
            GqlObj::PullPayload {
                owner,
                name,
                number,
            } => match field {
                "pullRequest" => Ok(Resolved::Object(GqlObj::Pull {
                    owner: owner.clone(),
                    name: name.clone(),
                    number: *number,
                })),
                _ => Err(format!(
                    "Cannot query field \"{field}\" on type \"CreatePullRequestPayload\"."
                )),
            },
            GqlObj::CommentPayload { id, body } => match field {
                "commentEdge" => Ok(Resolved::Object(GqlObj::CommentEdge {
                    id: id.clone(),
                    body: body.clone(),
                })),
                _ => Err(format!(
                    "Cannot query field \"{field}\" on type \"AddCommentPayload\"."
                )),
            },
        }
    }

    fn resolve_query(&mut self, field: &str, args: &[(String, Json)]) -> Result<Resolved, String> {
        match field {
            "viewer" => Ok(Resolved::Object(GqlObj::Viewer)),
            "repository" => {
                let owner = arg_str(args, "owner").unwrap_or("");
                let name = arg_str(args, "name").unwrap_or("");
                match self.store.get_repo(owner, name) {
                    Some(_) => Ok(Resolved::Object(GqlObj::Repo {
                        owner: owner.to_string(),
                        name: name.to_string(),
                    })),
                    None => Ok(Resolved::Null),
                }
            }
            "node" => {
                let id = arg_str(args, "id").unwrap_or("");
                let Some((kind, db_id, number)) = self.store.decode_gid(id) else {
                    return Ok(Resolved::Null);
                };
                let Some((owner, name)) = repo_key_by_db_id(self.store, db_id) else {
                    return Ok(Resolved::Null);
                };
                match kind.as_str() {
                    "Repository" => Ok(Resolved::Object(GqlObj::Repo { owner, name })),
                    "Issue" => Ok(Resolved::Object(GqlObj::Issue {
                        owner,
                        name,
                        number,
                    })),
                    "PullRequest" => Ok(Resolved::Object(GqlObj::Pull {
                        owner,
                        name,
                        number,
                    })),
                    _ => Ok(Resolved::Null),
                }
            }
            "__type" => {
                let name = arg_str(args, "name").unwrap_or("");
                if fields_of(name).is_some() || is_scalar(name) {
                    Ok(Resolved::Object(GqlObj::TypeInfo {
                        name: name.to_string(),
                    }))
                } else {
                    Ok(Resolved::Null)
                }
            }
            _ => Err(format!("Cannot query field \"{field}\" on type \"Query\".")),
        }
    }

    fn resolve_user(&mut self, field: &str) -> Result<Resolved, String> {
        match field {
            "login" => Ok(Resolved::Value(Json::Str(self.store.viewer.login.clone()))),
            "id" => Ok(Resolved::Value(Json::Str(base64_encode(
                format!("User:{}", self.store.viewer.login).as_bytes(),
            )))),
            "name" => Ok(Resolved::Value(Json::Str(self.store.viewer.name.clone()))),
            "email" => Ok(Resolved::Value(Json::Str(self.store.viewer.email.clone()))),
            _ => Err(format!("Cannot query field \"{field}\" on type \"User\".")),
        }
    }

    fn resolve_repo(
        &mut self,
        owner: &str,
        name: &str,
        field: &str,
        args: &[(String, Json)],
    ) -> Result<Resolved, String> {
        // Snapshot the scalar facts first so later branches can borrow the
        // store mutably for connections.
        let Some(repo) = self.store.get_repo(owner, name) else {
            return Ok(Resolved::Null);
        };
        let db_id = repo.db_id;
        let repo_name = repo.name.clone();
        let full_name = repo.full_name();
        let description = repo.description.clone();
        let is_private = repo.private;
        let default_branch = repo.default_branch.clone();
        let has_default = repo.default_tip().is_some();
        match field {
            "id" => Ok(Resolved::Value(Json::Str(self.store.repo_gid(repo)))),
            "databaseId" => Ok(Resolved::Value(Json::Int(db_id as i64))),
            "name" => Ok(Resolved::Value(Json::Str(repo_name))),
            "nameWithOwner" => Ok(Resolved::Value(Json::Str(full_name))),
            "owner" => Ok(Resolved::Object(GqlObj::Viewer)),
            "defaultBranchRef" => {
                if has_default {
                    Ok(Resolved::Object(GqlObj::BranchRef {
                        branch: default_branch,
                    }))
                } else {
                    Ok(Resolved::Null)
                }
            }
            "viewerPermission" => Ok(Resolved::Value(Json::Str("ADMIN".to_string()))),
            "hasIssuesEnabled" => Ok(Resolved::Value(Json::Bool(true))),
            "description" => Ok(Resolved::Value(Json::Str(description))),
            "url" => Ok(Resolved::Value(Json::Str(format!(
                "https://github.com/{full_name}"
            )))),
            "isPrivate" => Ok(Resolved::Value(Json::Bool(is_private))),
            "issue" => {
                let number = arg_int(args, "number").unwrap_or(0) as u32;
                let repo = self.store.get_repo(owner, name).unwrap();
                match repo.issues.iter().find(|issue| issue.number == number) {
                    Some(_) => Ok(Resolved::Object(GqlObj::Issue {
                        owner: owner.to_string(),
                        name: name.to_string(),
                        number,
                    })),
                    None => Ok(Resolved::Null),
                }
            }
            "issues" => Ok(Resolved::Object(self.issue_connection(owner, name, args))),
            "pullRequests" => {
                let first = arg_int(args, "first").unwrap_or(i64::MAX).max(0) as usize;
                let repo = self.store.get_repo(owner, name).unwrap();
                let numbers: Vec<u32> = repo
                    .pulls
                    .iter()
                    .filter(|pull| pull.state == IssueState::Open)
                    .map(|pull| pull.number)
                    .take(first)
                    .collect();
                Ok(Resolved::Object(GqlObj::PullConn {
                    owner: owner.to_string(),
                    name: name.to_string(),
                    numbers,
                }))
            }
            "pullRequest" => {
                let number = arg_int(args, "number").unwrap_or(0) as u32;
                let repo = self.store.get_repo(owner, name).unwrap();
                match repo.pulls.iter().find(|pull| pull.number == number) {
                    Some(_) => Ok(Resolved::Object(GqlObj::Pull {
                        owner: owner.to_string(),
                        name: name.to_string(),
                        number,
                    })),
                    None => Ok(Resolved::Null),
                }
            }
            _ => Err(format!(
                "Cannot query field \"{field}\" on type \"Repository\"."
            )),
        }
    }

    /// Build an issue connection: states filter, orderBy, cursor, first.
    fn issue_connection(&mut self, owner: &str, name: &str, args: &[(String, Json)]) -> GqlObj {
        let repo = self.store.get_repo(owner, name).unwrap();
        let mut numbers: Vec<(u32, u64)> = repo
            .issues
            .iter()
            .map(|issue| (issue.number, issue.created_seq))
            .collect();
        // States filter.
        if let Some(states) = args.iter().find(|(key, _)| key == "states") {
            if let Json::Arr(wanted) = &states.1 {
                let want_open = wanted.iter().any(|state| state.as_str() == Some("OPEN"));
                let want_closed = wanted.iter().any(|state| state.as_str() == Some("CLOSED"));
                numbers.retain(|(number, _)| {
                    let issue = repo.issues.iter().find(|i| i.number == *number).unwrap();
                    (want_open && issue.state == IssueState::Open)
                        || (want_closed && issue.state == IssueState::Closed)
                });
            }
        }
        // Order: CREATED_AT ASC is the default; DESC reverses it.
        let mut descending = false;
        if let Some(order_by) = arg_obj(args, "orderBy") {
            if obj_field(order_by, "direction").and_then(|v| v.as_str()) == Some("DESC") {
                descending = true;
            }
        }
        if descending {
            numbers.sort_by_key(|(_, seq)| std::cmp::Reverse(*seq));
        } else {
            numbers.sort_by_key(|(_, seq)| *seq);
        }
        // Cursor: start after the issue the cursor names, in this order.
        let mut start = 0;
        if let Some(after) = arg_str(args, "after") {
            if let Some(cursor_no) = cursor_number(after) {
                if let Some(pos) = numbers.iter().position(|(n, _)| *n == cursor_no) {
                    start = pos + 1;
                }
            }
        }
        let first = arg_int(args, "first").unwrap_or(i64::MAX).max(0) as usize;
        let page: Vec<u32> = numbers
            .iter()
            .skip(start)
            .take(first)
            .map(|(number, _)| *number)
            .collect();
        let has_next = start + page.len() < numbers.len();
        let end_cursor = page.last().map(|number| cursor_for(*number));
        GqlObj::IssueConn {
            owner: owner.to_string(),
            name: name.to_string(),
            numbers: page,
            has_next,
            end_cursor,
        }
    }

    fn resolve_issue_conn(
        &mut self,
        owner: &str,
        name: &str,
        numbers: &[u32],
        has_next: bool,
        end_cursor: Option<String>,
        field: &str,
    ) -> Result<Resolved, String> {
        match field {
            "nodes" => Ok(Resolved::Objects(
                numbers
                    .iter()
                    .map(|number| GqlObj::Issue {
                        owner: owner.to_string(),
                        name: name.to_string(),
                        number: *number,
                    })
                    .collect(),
            )),
            "edges" => Ok(Resolved::Objects(
                numbers
                    .iter()
                    .map(|number| GqlObj::IssueEdge {
                        owner: owner.to_string(),
                        name: name.to_string(),
                        number: *number,
                        cursor: cursor_for(*number),
                    })
                    .collect(),
            )),
            "pageInfo" => Ok(Resolved::Object(GqlObj::PageInfo {
                has_next,
                has_prev: false,
                start_cursor: numbers.first().map(|number| cursor_for(*number)),
                end_cursor,
            })),
            "totalCount" => Ok(Resolved::Value(Json::Int(numbers.len() as i64))),
            _ => Err(format!(
                "Cannot query field \"{field}\" on type \"IssueConnection\"."
            )),
        }
    }

    fn resolve_pull_conn(
        &mut self,
        owner: &str,
        name: &str,
        numbers: &[u32],
        field: &str,
    ) -> Result<Resolved, String> {
        match field {
            "nodes" => Ok(Resolved::Objects(
                numbers
                    .iter()
                    .map(|number| GqlObj::Pull {
                        owner: owner.to_string(),
                        name: name.to_string(),
                        number: *number,
                    })
                    .collect(),
            )),
            "edges" => Ok(Resolved::Objects(
                numbers
                    .iter()
                    .map(|number| GqlObj::PullEdge {
                        owner: owner.to_string(),
                        name: name.to_string(),
                        number: *number,
                        cursor: cursor_for(*number),
                    })
                    .collect(),
            )),
            "pageInfo" => Ok(Resolved::Object(GqlObj::PageInfo {
                has_next: false,
                has_prev: false,
                start_cursor: numbers.first().map(|number| cursor_for(*number)),
                end_cursor: numbers.last().map(|number| cursor_for(*number)),
            })),
            "totalCount" => Ok(Resolved::Value(Json::Int(numbers.len() as i64))),
            _ => Err(format!(
                "Cannot query field \"{field}\" on type \"PullRequestConnection\"."
            )),
        }
    }

    fn resolve_issue(
        &mut self,
        owner: &str,
        name: &str,
        number: u32,
        field: &str,
    ) -> Result<Resolved, String> {
        let Some(repo) = self.store.get_repo(owner, name) else {
            return Ok(Resolved::Null);
        };
        let Some(issue) = repo.issues.iter().find(|issue| issue.number == number) else {
            return Ok(Resolved::Null);
        };
        let gid = self.store.issue_gid(repo, number);
        let state = if issue.state == IssueState::Open {
            "OPEN"
        } else {
            "CLOSED"
        };
        match field {
            "id" => Ok(Resolved::Value(Json::Str(gid))),
            "number" => Ok(Resolved::Value(Json::Int(number as i64))),
            "title" => Ok(Resolved::Value(Json::Str(issue.title.clone()))),
            "state" => Ok(Resolved::Value(Json::Str(state.to_string()))),
            "body" => Ok(Resolved::Value(Json::Str(issue.body.clone()))),
            "url" => Ok(Resolved::Value(Json::Str(format!(
                "https://github.com/{}/{}/issues/{}",
                owner, name, number
            )))),
            "createdAt" => Ok(Resolved::Value(Json::Str(iso_from_epoch(
                SCENARIO_EPOCH_SEC + issue.created_seq,
            )))),
            "author" => Ok(Resolved::Object(GqlObj::Viewer)),
            "labels" => Ok(Resolved::Objects(
                issue
                    .labels
                    .iter()
                    .filter_map(|wanted| {
                        repo.labels
                            .iter()
                            .find(|label| &label.name == wanted)
                            .map(|label| GqlObj::Label {
                                name: label.name.clone(),
                                color: label.color.clone(),
                            })
                    })
                    .collect(),
            )),
            _ => Err(format!("Cannot query field \"{field}\" on type \"Issue\".")),
        }
    }

    fn resolve_pull(
        &mut self,
        owner: &str,
        name: &str,
        number: u32,
        field: &str,
        args: &[(String, Json)],
    ) -> Result<Resolved, String> {
        let Some(repo) = self.store.get_repo(owner, name) else {
            return Ok(Resolved::Null);
        };
        let Some(pull) = repo.pulls.iter().find(|pull| pull.number == number) else {
            return Ok(Resolved::Null);
        };
        let gid = self.store.pull_gid(repo, number);
        let state = if pull.merged {
            "MERGED"
        } else if pull.state == IssueState::Open {
            "OPEN"
        } else {
            "CLOSED"
        };
        match field {
            "id" => Ok(Resolved::Value(Json::Str(gid))),
            "number" => Ok(Resolved::Value(Json::Int(number as i64))),
            "title" => Ok(Resolved::Value(Json::Str(pull.title.clone()))),
            "body" => Ok(Resolved::Value(Json::Str(pull.body.clone()))),
            "state" => Ok(Resolved::Value(Json::Str(state.to_string()))),
            "merged" => Ok(Resolved::Value(Json::Bool(pull.merged))),
            "headRefName" => Ok(Resolved::Value(Json::Str(pull.head_ref.clone()))),
            "baseRefName" => Ok(Resolved::Value(Json::Str(pull.base_ref.clone()))),
            "mergeable" => {
                let mergeable = crate::store::pull_mergeable(repo, pull);
                Ok(Resolved::Value(Json::Str(mergeable.as_str().to_string())))
            }
            "files" => {
                let first = arg_int(args, "first").unwrap_or(i64::MAX).max(0) as usize;
                let Some(head_tip) = repo.branch_tip(&pull.head_ref) else {
                    return Ok(Resolved::Null);
                };
                let Some(base_commit) = repo.objects.commit_of(&pull.base_sha) else {
                    return Ok(Resolved::Null);
                };
                let Some(head_commit) = repo.objects.commit_of(&head_tip) else {
                    return Ok(Resolved::Null);
                };
                let mut changes =
                    crate::store::diff_trees(repo, base_commit.tree, head_commit.tree);
                changes.truncate(first);
                Ok(Resolved::Object(GqlObj::FileConn { files: changes }))
            }
            "url" => Ok(Resolved::Value(Json::Str(format!(
                "https://github.com/{}/{}/pull/{}",
                owner, name, number
            )))),
            "createdAt" => Ok(Resolved::Value(Json::Str(iso_from_epoch(
                SCENARIO_EPOCH_SEC + pull.created_seq,
            )))),
            "author" => Ok(Resolved::Object(GqlObj::Viewer)),
            _ => Err(format!(
                "Cannot query field \"{field}\" on type \"PullRequest\"."
            )),
        }
    }

    fn resolve_type_info(&mut self, name: &str, field: &str) -> Result<Resolved, String> {
        match field {
            "name" => Ok(Resolved::Value(Json::Str(name.to_string()))),
            "kind" => Ok(Resolved::Value(Json::Str(type_kind(name).to_string()))),
            "description" => Ok(Resolved::Null),
            "fields" => {
                let Some(fields) = fields_of(name) else {
                    return Ok(Resolved::Null);
                };
                Ok(Resolved::Objects(
                    fields
                        .iter()
                        .map(|(field_name, _)| GqlObj::FieldInfo {
                            name: field_name.to_string(),
                        })
                        .collect(),
                ))
            }
            "enumValues" => {
                if !is_enum(name) {
                    return Ok(Resolved::Null);
                }
                Ok(Resolved::Objects(
                    enum_values(name)
                        .iter()
                        .map(|value| GqlObj::EnumValueInfo {
                            name: value.to_string(),
                        })
                        .collect(),
                ))
            }
            _ => Err(format!(
                "Cannot query field \"{field}\" on type \"__Type\"."
            )),
        }
    }

    // -- mutations --------------------------------------------------------

    fn resolve_mutation(
        &mut self,
        field: &str,
        args: &[(String, Json)],
    ) -> Result<Resolved, String> {
        match field {
            "createIssue" => self.mut_create_issue(args),
            "closeIssue" => self.mut_close_issue(args),
            "createPullRequest" => self.mut_create_pull_request(args),
            "mergePullRequest" => self.mut_merge_pull_request(args),
            "addComment" => self.mut_add_comment(args),
            _ => Err(format!(
                "Cannot query field \"{field}\" on type \"Mutation\"."
            )),
        }
    }

    fn mut_create_issue(&mut self, args: &[(String, Json)]) -> Result<Resolved, String> {
        let input = arg_obj(args, "input").cloned().unwrap_or(Json::Null);
        let repo_id = obj_field(&input, "repositoryId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let title = obj_field(&input, "title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let body = obj_field(&input, "body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if title.is_empty() {
            return Err("createIssue: title is required".to_string());
        }
        let Some((_, db_id, _)) = self.store.decode_gid(repo_id) else {
            return Err("createIssue: unknown repositoryId".to_string());
        };
        let Some(key) = repo_key_by_db_id(self.store, db_id) else {
            return Err("createIssue: unknown repositoryId".to_string());
        };
        let id = self.store.next_issue_id;
        self.store.next_issue_id += 1;
        self.store.created_seq += 1;
        let created_seq = self.store.created_seq;
        let repo = self.store.repos.get_mut(&key).unwrap();
        repo.number_seq += 1;
        let number = repo.number_seq;
        repo.issues.push(Issue {
            id,
            number,
            title,
            body,
            state: IssueState::Open,
            labels: Vec::new(),
            comments: Vec::new(),
            created_seq,
        });
        Ok(Resolved::Object(GqlObj::IssuePayload {
            owner: key.0,
            name: key.1,
            number,
        }))
    }

    fn mut_close_issue(&mut self, args: &[(String, Json)]) -> Result<Resolved, String> {
        let input = arg_obj(args, "input").cloned().unwrap_or(Json::Null);
        let issue_id = obj_field(&input, "issueId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let Some((_, db_id, number)) = self.store.decode_gid(issue_id) else {
            return Err("closeIssue: unknown issueId".to_string());
        };
        let Some(key) = repo_key_by_db_id(self.store, db_id) else {
            return Err("closeIssue: unknown issueId".to_string());
        };
        let repo = self.store.repos.get_mut(&key).unwrap();
        let Some(issue) = repo.issues.iter_mut().find(|issue| issue.number == number) else {
            return Err("closeIssue: unknown issueId".to_string());
        };
        issue.state = IssueState::Closed;
        Ok(Resolved::Object(GqlObj::IssuePayload {
            owner: key.0,
            name: key.1,
            number,
        }))
    }

    fn mut_create_pull_request(&mut self, args: &[(String, Json)]) -> Result<Resolved, String> {
        let input = arg_obj(args, "input").cloned().unwrap_or(Json::Null);
        let repo_id = obj_field(&input, "repositoryId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let base_ref = obj_field(&input, "baseRefName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let head_ref = obj_field(&input, "headRefName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let title = obj_field(&input, "title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let Some((_, db_id, _)) = self.store.decode_gid(repo_id) else {
            return Err("createPullRequest: unknown repositoryId".to_string());
        };
        let Some(key) = repo_key_by_db_id(self.store, db_id) else {
            return Err("createPullRequest: unknown repositoryId".to_string());
        };
        let (base_sha, head_ok) = match self.store.get_repo(&key.0, &key.1) {
            Some(repo) => (
                repo.branch_tip(&base_ref),
                repo.branch_tip(&head_ref).is_some(),
            ),
            None => return Err("createPullRequest: unknown repositoryId".to_string()),
        };
        let Some(base_sha) = base_sha else {
            return Err("createPullRequest: base branch does not exist".to_string());
        };
        if !head_ok {
            return Err("createPullRequest: head branch does not exist".to_string());
        }
        if self.store.get_repo(&key.0, &key.1).is_some_and(|repo| {
            repo.pulls
                .iter()
                .any(|pull| pull.head_ref == head_ref && pull.state == IssueState::Open)
        }) {
            return Err(
                "createPullRequest: a pull request from this head branch is already open"
                    .to_string(),
            );
        }
        let id = self.store.next_pull_id;
        self.store.next_pull_id += 1;
        self.store.created_seq += 1;
        let created_seq = self.store.created_seq;
        let repo = self.store.repos.get_mut(&key).unwrap();
        repo.number_seq += 1;
        let number = repo.number_seq;
        repo.pulls.push(Pull {
            id,
            number,
            title,
            body: String::new(),
            head_ref,
            base_ref,
            base_sha,
            state: IssueState::Open,
            merged: false,
            merged_at: None,
            merge_commit_sha: None,
            created_seq,
        });
        Ok(Resolved::Object(GqlObj::PullPayload {
            owner: key.0,
            name: key.1,
            number,
        }))
    }

    fn mut_merge_pull_request(&mut self, args: &[(String, Json)]) -> Result<Resolved, String> {
        let input = arg_obj(args, "input").cloned().unwrap_or(Json::Null);
        let pull_id = obj_field(&input, "pullRequestId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let method = obj_field(&input, "mergeMethod")
            .and_then(|v| v.as_str())
            .unwrap_or("SQUASH");
        if !matches!(method, "MERGE" | "SQUASH" | "REBASE") {
            return Err(format!("mergePullRequest: unknown mergeMethod {method}"));
        }
        let Some((_, db_id, number)) = self.store.decode_gid(pull_id) else {
            return Err("mergePullRequest: unknown pullRequestId".to_string());
        };
        let Some(key) = repo_key_by_db_id(self.store, db_id) else {
            return Err("mergePullRequest: unknown pullRequestId".to_string());
        };
        let identity = self.store.identity();
        let timestamp = self.store.next_commit_timestamp();
        let merged_at = iso_from_epoch(SCENARIO_EPOCH_SEC + self.store.commit_seq);
        let repo = self.store.repos.get_mut(&key).unwrap();
        match squash_merge(repo, &identity, timestamp, merged_at, number) {
            Ok(_) => Ok(Resolved::Object(GqlObj::PullPayload {
                owner: key.0,
                name: key.1,
                number,
            })),
            Err("conflict") => Err("mergePullRequest: merge conflict".to_string()),
            Err("already merged") => Err("mergePullRequest: already merged".to_string()),
            Err("closed") => Err("mergePullRequest: pull request is closed".to_string()),
            Err(_) => Err("mergePullRequest: pull request cannot be merged".to_string()),
        }
    }

    fn mut_add_comment(&mut self, args: &[(String, Json)]) -> Result<Resolved, String> {
        let input = arg_obj(args, "input").cloned().unwrap_or(Json::Null);
        let subject_id = obj_field(&input, "subjectId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let body = obj_field(&input, "body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let Some((kind, db_id, number)) = self.store.decode_gid(subject_id) else {
            return Err("addComment: unknown subjectId".to_string());
        };
        if kind != "Issue" {
            return Err("addComment: subject is not an issue".to_string());
        }
        let Some(key) = repo_key_by_db_id(self.store, db_id) else {
            return Err("addComment: unknown subjectId".to_string());
        };
        let repo = self.store.repos.get_mut(&key).unwrap();
        repo.comment_seq += 1;
        let comment_id = repo.comment_seq;
        let Some(issue) = repo.issues.iter_mut().find(|issue| issue.number == number) else {
            return Err("addComment: unknown subjectId".to_string());
        };
        issue.comments.push(crate::store::Comment {
            id: comment_id,
            body: body.clone(),
        });
        let comment_gid = base64_encode(format!("IssueComment:{db_id}:{comment_id}").as_bytes());
        Ok(Resolved::Object(GqlObj::CommentPayload {
            id: comment_gid,
            body,
        }))
    }
}

// ---------------------------------------------------------------------------
// HTTP entry point.

/// Serve one GraphQL request. Accepts a single `{query, variables}`
/// object or a JSON array of them (batch); answers 200 with
/// `{"data": ...}` (plus an `errors` array when validation or execution
/// reported any), or 400 when the body is not JSON at all.
pub fn handle(store: &RwLock<Store>, req: &Request) -> Response {
    if req.method != "POST" {
        return error_reply(405, "method not allowed");
    }
    let body_text = String::from_utf8_lossy(&req.body);
    let parsed = match crate::json::parse(&body_text) {
        Ok(value) => value,
        Err(_) => return error_reply(400, "malformed JSON body"),
    };
    let mut guard = store.write().unwrap();
    let is_batch = matches!(parsed, Json::Arr(_));
    let payloads: Vec<&Json> = match &parsed {
        Json::Arr(items) => items.iter().collect(),
        other => vec![other],
    };
    let mut results = Vec::with_capacity(payloads.len());
    for payload in payloads {
        results.push(run_one(&mut guard, payload));
    }
    let body = if is_batch {
        Json::Arr(results)
    } else {
        results.into_iter().next().unwrap_or(Json::Null)
    };
    Response::new(200, body.stringify().into_bytes())
        .header("content-type", "application/json; charset=utf-8")
}

fn error_reply(status: u16, message: &str) -> Response {
    let body = crate::json::obj(vec![("message", Json::Str(message.to_string()))]);
    Response::new(status, body.stringify().into_bytes())
        .header("content-type", "application/json; charset=utf-8")
}

/// `{"data": null, "errors": [{"message": ...}]}`.
fn error_result(message: &str) -> Json {
    crate::json::obj(vec![
        ("data", Json::Null),
        (
            "errors",
            Json::Arr(vec![crate::json::obj(vec![(
                "message",
                Json::Str(message.to_string()),
            )])]),
        ),
    ])
}

/// Run one `{query, variables, operationName}` payload against the store.
fn run_one(store: &mut Store, payload: &Json) -> Json {
    let Some(query) = payload.get("query").and_then(|value| value.as_str()) else {
        return error_result("GraphQL request must include a query");
    };
    let doc = match parse_document(query) {
        Ok(doc) => doc,
        Err(message) => return error_result(&message),
    };
    let operation_name = payload
        .get("operationName")
        .and_then(|value| value.as_str());
    let op = match pick_operation(&doc, operation_name) {
        Ok(op) => op,
        Err(message) => return error_result(&message),
    };
    // Variables: defaults from the operation's variable definitions,
    // overlaid by the values the request supplied.
    let mut vars: HashMap<String, Json> = HashMap::new();
    for (name, default) in &op.var_defs {
        if let Some(default) = default {
            vars.insert(name.clone(), default.resolve(&HashMap::new()));
        }
    }
    if let Some(Json::Obj(entries)) = payload.get("variables") {
        for (key, value) in entries {
            vars.insert(key.clone(), value.clone());
        }
    }
    let validation_errors = validate_operation(&doc, op);
    if !validation_errors.is_empty() {
        let errors: Vec<Json> = validation_errors
            .iter()
            .map(|message| crate::json::obj(vec![("message", Json::Str(message.clone()))]))
            .collect();
        return crate::json::obj(vec![("data", Json::Null), ("errors", Json::Arr(errors))]);
    }
    let mut exec = Exec {
        store,
        doc: &doc,
        vars,
        errors: Vec::new(),
        frag_stack: Vec::new(),
        frag_budget: 0,
    };
    let data = exec.run(op);
    let errors = std::mem::take(&mut exec.errors);
    if errors.is_empty() {
        crate::json::obj(vec![("data", data)])
    } else {
        let errors_json: Vec<Json> = errors
            .iter()
            .map(|message| crate::json::obj(vec![("message", Json::Str(message.clone()))]))
            .collect();
        crate::json::obj(vec![("data", data), ("errors", Json::Arr(errors_json))])
    }
}

fn pick_operation<'d>(doc: &'d Document, name: Option<&str>) -> Result<&'d Operation, String> {
    if let Some(name) = name {
        return doc
            .ops
            .iter()
            .find(|op| op.name.as_deref() == Some(name))
            .ok_or_else(|| format!("Unknown operation named \"{name}\"."));
    }
    if doc.ops.len() == 1 {
        return Ok(&doc.ops[0]);
    }
    Err("Must provide operation name if query contains multiple operations.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::build_scenario;

    fn test_store() -> Store {
        let mut store = Store::new(build_scenario().viewer);
        store
            .create_repo("demo", "demo repo", false)
            .expect("create demo repo");
        store
    }

    /// Run a GraphQL document against a store, with optional variables.
    fn gql(store: &mut Store, query: &str, variables: Option<Json>) -> Json {
        let mut payload = crate::json::obj(vec![("query", Json::Str(query.to_string()))]);
        if let Some(variables) = variables {
            if let Json::Obj(entries) = &mut payload {
                entries.push(("variables".to_string(), variables));
            }
        }
        run_one(store, &payload)
    }

    fn data(result: &Json) -> &Json {
        result.get("data").expect("result has data")
    }

    fn errors_of(result: &Json) -> Vec<String> {
        match result.get("errors") {
            Some(Json::Arr(items)) => items
                .iter()
                .filter_map(|item| item.get("message"))
                .filter_map(|message| message.as_str())
                .map(|message| message.to_string())
                .collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn viewer_login() {
        let mut store = test_store();
        let result = gql(&mut store, "{ viewer { login } }", None);
        assert!(errors_of(&result).is_empty(), "{result:?}");
        assert_eq!(
            data(&result)
                .get("viewer")
                .unwrap()
                .get("login")
                .unwrap()
                .as_str(),
            Some("adaiversen")
        );
    }

    #[test]
    fn repository_fields() {
        let mut store = test_store();
        let query = "query RepositoryFields($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { id databaseId name nameWithOwner owner { login } defaultBranchRef { name } viewerPermission hasIssuesEnabled } }";
        let variables = crate::json::obj(vec![
            ("owner", Json::Str("adaiversen".to_string())),
            ("name", Json::Str("demo".to_string())),
        ]);
        let result = gql(&mut store, query, Some(variables));
        assert!(errors_of(&result).is_empty(), "{result:?}");
        let repo = data(&result).get("repository").unwrap();
        let id = repo.get("id").unwrap().as_str().unwrap();
        assert!(!id.is_empty());
        assert_ne!(id, "1", "global id must not be the plain database id");
        assert!(repo.get("databaseId").unwrap().as_i64().unwrap() > 0);
        assert_eq!(repo.get("name").unwrap().as_str(), Some("demo"));
        assert_eq!(
            repo.get("nameWithOwner").unwrap().as_str(),
            Some("adaiversen/demo")
        );
        assert_eq!(
            repo.get("owner").unwrap().get("login").unwrap().as_str(),
            Some("adaiversen")
        );
        assert_eq!(
            repo.get("viewerPermission").unwrap().as_str(),
            Some("ADMIN")
        );
        assert_eq!(repo.get("hasIssuesEnabled").unwrap().as_bool(), Some(true));
        // The default branch ref is null on an empty repo; a second read
        // must return the same global id.
        let again = gql(
            &mut store,
            query,
            Some(crate::json::obj(vec![
                ("owner", Json::Str("adaiversen".to_string())),
                ("name", Json::Str("demo".to_string())),
            ])),
        );
        assert_eq!(
            data(&again)
                .get("repository")
                .unwrap()
                .get("id")
                .unwrap()
                .as_str(),
            Some(id)
        );
    }

    #[test]
    fn create_issue_close_issue_and_read_back() {
        let mut store = test_store();
        let repo_id = store
            .repo_gid(store.get_repo("adaiversen", "demo").unwrap())
            .clone();
        let create = "mutation CreateIssue($input: CreateIssueInput!) { createIssue(input: $input) { issue { number id title state } } }";
        let variables = crate::json::obj(vec![(
            "input",
            crate::json::obj(vec![
                ("repositoryId", Json::Str(repo_id.clone())),
                ("title", Json::Str("First bug".to_string())),
                ("body", Json::Str("It broke".to_string())),
            ]),
        )]);
        let result = gql(&mut store, create, Some(variables));
        assert!(errors_of(&result).is_empty(), "{result:?}");
        let issue = data(&result)
            .get("createIssue")
            .unwrap()
            .get("issue")
            .unwrap();
        assert_eq!(issue.get("number").unwrap().as_i64(), Some(1));
        assert_eq!(issue.get("title").unwrap().as_str(), Some("First bug"));
        assert_eq!(issue.get("state").unwrap().as_str(), Some("OPEN"));
        let issue_id = issue.get("id").unwrap().as_str().unwrap().to_string();

        let read = gql(
            &mut store,
            "query IssueByNumber($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { issue(number: $number) { number title state body } } }",
            Some(crate::json::obj(vec![
                ("owner", Json::Str("adaiversen".to_string())),
                ("name", Json::Str("demo".to_string())),
                ("number", Json::Int(1)),
            ])),
        );
        assert!(errors_of(&read).is_empty(), "{read:?}");
        let found = data(&read).get("repository").unwrap().get("issue").unwrap();
        assert_eq!(found.get("title").unwrap().as_str(), Some("First bug"));
        assert_eq!(found.get("state").unwrap().as_str(), Some("OPEN"));

        let close = gql(
            &mut store,
            "mutation CloseIssue($input: CloseIssueInput!) { closeIssue(input: $input) { issue { number state } } }",
            Some(crate::json::obj(vec![(
                "input",
                crate::json::obj(vec![("issueId", Json::Str(issue_id))]),
            )])),
        );
        assert!(errors_of(&close).is_empty(), "{close:?}");
        assert_eq!(
            data(&close)
                .get("closeIssue")
                .unwrap()
                .get("issue")
                .unwrap()
                .get("state")
                .unwrap()
                .as_str(),
            Some("CLOSED")
        );
    }

    #[test]
    fn unknown_field_reports_error_naming_the_field() {
        let mut store = test_store();
        let result = gql(&mut store, "{ viewer { login noSuchFieldOnViewer } }", None);
        let messages = errors_of(&result);
        assert!(!messages.is_empty(), "expected errors, got {result:?}");
        assert!(
            messages.iter().any(|m| m.contains("noSuchFieldOnViewer")),
            "messages: {messages:?}"
        );
        assert_eq!(data(&result), &Json::Null);
    }

    #[test]
    fn unknown_repository_returns_null_without_errors() {
        let mut store = test_store();
        let result = gql(
            &mut store,
            "query { repository(owner: \"adaiversen\", name: \"nope\") { id name } }",
            None,
        );
        assert!(errors_of(&result).is_empty(), "{result:?}");
        assert_eq!(data(&result).get("repository").unwrap(), &Json::Null);
    }

    #[test]
    fn node_round_trips_repository() {
        let mut store = test_store();
        let repo_id = store
            .repo_gid(store.get_repo("adaiversen", "demo").unwrap())
            .clone();
        let result = gql(
            &mut store,
            "query NodeById($id: ID!) { node(id: $id) { id ... on Repository { name nameWithOwner } } }",
            Some(crate::json::obj(vec![(
                "id",
                Json::Str(repo_id),
            )])),
        );
        assert!(errors_of(&result).is_empty(), "{result:?}");
        let node = data(&result).get("node").unwrap();
        assert_eq!(node.get("name").unwrap().as_str(), Some("demo"));
        assert_eq!(
            node.get("nameWithOwner").unwrap().as_str(),
            Some("adaiversen/demo")
        );
    }

    #[test]
    fn issues_pagination_with_cursors() {
        let mut store = test_store();
        let repo_id = store
            .repo_gid(store.get_repo("adaiversen", "demo").unwrap())
            .clone();
        for n in 1..=7 {
            let result = gql(
                &mut store,
                "mutation CreateIssue($input: CreateIssueInput!) { createIssue(input: $input) { issue { number } } }",
                Some(crate::json::obj(vec![(
                    "input",
                    crate::json::obj(vec![
                        ("repositoryId", Json::Str(repo_id.clone())),
                        ("title", Json::Str(format!("issue {n}"))),
                    ]),
                )])),
            );
            assert!(errors_of(&result).is_empty(), "{result:?}");
        }
        let page_query = "query IssuesPage($owner: String!, $name: String!, $first: Int, $after: String) { repository(owner: $owner, name: $name) { issues(first: $first, after: $after) { nodes { number } pageInfo { hasNextPage endCursor } } } }";
        let page1 = gql(
            &mut store,
            page_query,
            Some(crate::json::obj(vec![
                ("owner", Json::Str("adaiversen".to_string())),
                ("name", Json::Str("demo".to_string())),
                ("first", Json::Int(5)),
            ])),
        );
        assert!(errors_of(&page1).is_empty(), "{page1:?}");
        let issues1 = data(&page1)
            .get("repository")
            .unwrap()
            .get("issues")
            .unwrap();
        let nodes1 = issues1.get("nodes").unwrap().as_arr().unwrap();
        assert_eq!(nodes1.len(), 5);
        assert_eq!(
            issues1
                .get("pageInfo")
                .unwrap()
                .get("hasNextPage")
                .unwrap()
                .as_bool(),
            Some(true)
        );
        let cursor = issues1
            .get("pageInfo")
            .unwrap()
            .get("endCursor")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert!(!cursor.is_empty());
        let page2 = gql(
            &mut store,
            page_query,
            Some(crate::json::obj(vec![
                ("owner", Json::Str("adaiversen".to_string())),
                ("name", Json::Str("demo".to_string())),
                ("first", Json::Int(5)),
                ("after", Json::Str(cursor)),
            ])),
        );
        assert!(errors_of(&page2).is_empty(), "{page2:?}");
        let issues2 = data(&page2)
            .get("repository")
            .unwrap()
            .get("issues")
            .unwrap();
        let nodes2 = issues2.get("nodes").unwrap().as_arr().unwrap();
        assert_eq!(nodes2.len(), 2);
        assert_eq!(
            issues2
                .get("pageInfo")
                .unwrap()
                .get("hasNextPage")
                .unwrap()
                .as_bool(),
            Some(false)
        );
        let mut seen: Vec<i64> = nodes1
            .iter()
            .chain(nodes2.iter())
            .map(|node| node.get("number").unwrap().as_i64().unwrap())
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, vec![1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn issues_open_desc_order() {
        let mut store = test_store();
        let repo_id = store
            .repo_gid(store.get_repo("adaiversen", "demo").unwrap())
            .clone();
        for n in 1..=3 {
            gql(
                &mut store,
                "mutation CreateIssue($input: CreateIssueInput!) { createIssue(input: $input) { issue { number } } }",
                Some(crate::json::obj(vec![(
                    "input",
                    crate::json::obj(vec![
                        ("repositoryId", Json::Str(repo_id.clone())),
                        ("title", Json::Str(format!("issue {n}"))),
                    ]),
                )])),
            );
        }
        // Close issue 2 so only 1 and 3 stay open.
        let issue2_id = store
            .issue_gid(store.get_repo("adaiversen", "demo").unwrap(), 2)
            .clone();
        gql(
            &mut store,
            "mutation CloseIssue($input: CloseIssueInput!) { closeIssue(input: $input) { issue { number } } }",
            Some(crate::json::obj(vec![(
                "input",
                crate::json::obj(vec![("issueId", Json::Str(issue2_id))]),
            )])),
        );
        let result = gql(
            &mut store,
            "query OpenIssues($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { issues(first: 10, states: [OPEN], orderBy: {field: CREATED_AT, direction: DESC}) { nodes { number } } } }",
            Some(crate::json::obj(vec![
                ("owner", Json::Str("adaiversen".to_string())),
                ("name", Json::Str("demo".to_string())),
            ])),
        );
        assert!(errors_of(&result).is_empty(), "{result:?}");
        let nodes = data(&result)
            .get("repository")
            .unwrap()
            .get("issues")
            .unwrap()
            .get("nodes")
            .unwrap()
            .as_arr()
            .unwrap();
        let numbers: Vec<i64> = nodes
            .iter()
            .map(|node| node.get("number").unwrap().as_i64().unwrap())
            .collect();
        assert_eq!(numbers, vec![3, 1]);
    }

    #[test]
    fn fragment_cycle_rejected() {
        let mut store = test_store();
        let result = gql(
            &mut store,
            "query { viewer { ...F } } fragment F on User { login ...F }",
            None,
        );
        let messages = errors_of(&result);
        assert!(!messages.is_empty(), "expected errors, got {result:?}");
        assert!(
            messages
                .iter()
                .any(|m| m.contains("fragment cycle detected: F")),
            "messages: {messages:?}"
        );
    }

    #[test]
    fn deep_selection_rejected() {
        let mut store = test_store();
        // 200 nested inline fragments: the parser's depth cap (128)
        // must stop this before any execution happens.
        let mut query = String::from("query { ");
        for _ in 0..200 {
            query.push_str("... on Query { ");
        }
        query.push_str("viewer { login }");
        for _ in 0..200 {
            query.push_str(" }");
        }
        query.push_str(" }");
        let result = gql(&mut store, &query, None);
        let messages = errors_of(&result);
        assert!(
            messages
                .iter()
                .any(|m| m.contains("GraphQL selection nesting too deep")),
            "messages: {messages:?}"
        );
    }

    #[test]
    fn deep_value_rejected() {
        let mut store = test_store();
        // A 200-deep list literal as an argument value.
        let mut value = String::new();
        for _ in 0..200 {
            value.push('[');
        }
        value.push_str("\"OPEN\"");
        for _ in 0..200 {
            value.push(']');
        }
        let query = format!(
            "query {{ repository(owner: \"adaiversen\", name: \"demo\") {{ issues(states: {value}) {{ nodes {{ number }} }} }} }}"
        );
        let result = gql(&mut store, &query, None);
        let messages = errors_of(&result);
        assert!(
            messages
                .iter()
                .any(|m| m.contains("GraphQL value nesting too deep")),
            "messages: {messages:?}"
        );
    }

    #[test]
    fn introspection_lists_issue_fields() {
        let mut store = test_store();
        let result = gql(
            &mut store,
            "query { Issue: __type(name: \"Issue\") { fields(includeDeprecated: true) { name } } }",
            None,
        );
        assert!(errors_of(&result).is_empty(), "{result:?}");
        let fields = data(&result)
            .get("Issue")
            .unwrap()
            .get("fields")
            .unwrap()
            .as_arr()
            .unwrap();
        let names: Vec<&str> = fields
            .iter()
            .filter_map(|field| field.get("name"))
            .filter_map(|name| name.as_str())
            .collect();
        for wanted in ["id", "number", "title", "state", "body"] {
            assert!(names.contains(&wanted), "fields: {names:?}");
        }
    }

    #[test]
    fn introspection_search_type_has_issue() {
        let mut store = test_store();
        let result = gql(
            &mut store,
            "query { __type(name: \"SearchType\") { enumValues { name } } }",
            None,
        );
        assert!(errors_of(&result).is_empty(), "{result:?}");
        let values = data(&result)
            .get("__type")
            .unwrap()
            .get("enumValues")
            .unwrap()
            .as_arr()
            .unwrap();
        let names: Vec<&str> = values
            .iter()
            .filter_map(|value| value.get("name"))
            .filter_map(|name| name.as_str())
            .collect();
        assert!(names.contains(&"ISSUE"), "values: {names:?}");
    }

    #[test]
    fn batch_returns_results_in_order() {
        let store = std::sync::RwLock::new(test_store());
        let body = crate::json::parse(
            "[{\"query\":\"{ viewer { login } }\"},{\"query\":\"{ viewer { login } }\"}]",
        )
        .unwrap();
        let request = Request {
            method: "POST".to_string(),
            path: "/api/graphql".to_string(),
            query: Vec::new(),
            headers: Vec::new(),
            body: body.stringify().into_bytes(),
        };
        let response = handle(&store, &request);
        assert_eq!(response.status, 200);
        let parsed = crate::json::parse(&String::from_utf8_lossy(&response.body)).unwrap();
        let items = parsed.as_arr().unwrap();
        assert_eq!(items.len(), 2);
        for item in items {
            assert_eq!(
                item.get("data")
                    .unwrap()
                    .get("viewer")
                    .unwrap()
                    .get("login")
                    .unwrap()
                    .as_str(),
                Some("adaiversen")
            );
        }
    }

    #[test]
    fn malformed_json_body_is_400() {
        let store = std::sync::RwLock::new(test_store());
        let request = Request {
            method: "POST".to_string(),
            path: "/api/graphql".to_string(),
            query: Vec::new(),
            headers: Vec::new(),
            body: b"{not json".to_vec(),
        };
        let response = handle(&store, &request);
        assert_eq!(response.status, 400);
        let parsed = crate::json::parse(&String::from_utf8_lossy(&response.body)).unwrap();
        assert_eq!(
            parsed.get("message").unwrap().as_str(),
            Some("malformed JSON body")
        );
    }

    #[test]
    fn pull_request_flow_with_files_and_merge() {
        let mut store = test_store();
        let identity = store.identity();
        let timestamp = store.next_commit_timestamp();
        {
            let repo = store.get_repo_mut("adaiversen", "demo").unwrap();
            let default_branch = repo.default_branch.clone();
            crate::store::write_file(
                repo,
                &identity,
                timestamp,
                "README.md",
                b"hello\n",
                "init",
                Some(&default_branch),
                None,
            )
            .expect("write README");
            let tip = repo.branch_tip(&default_branch).unwrap();
            repo.set_ref("refs/heads/feature", Some(tip));
        }
        let timestamp = store.next_commit_timestamp();
        {
            let repo = store.get_repo_mut("adaiversen", "demo").unwrap();
            crate::store::write_file(
                repo,
                &identity,
                timestamp,
                "feature.txt",
                b"new\n",
                "add feature",
                Some("feature"),
                None,
            )
            .expect("write feature.txt");
        }
        let repo_id = store
            .repo_gid(store.get_repo("adaiversen", "demo").unwrap())
            .clone();
        let create = gql(
            &mut store,
            "mutation CreatePR($input: CreatePullRequestInput!) { createPullRequest(input: $input) { pullRequest { number id state headRefName baseRefName } } }",
            Some(crate::json::obj(vec![(
                "input",
                crate::json::obj(vec![
                    ("repositoryId", Json::Str(repo_id.clone())),
                    ("baseRefName", Json::Str("main".to_string())),
                    ("headRefName", Json::Str("feature".to_string())),
                    ("title", Json::Str("Add feature".to_string())),
                ]),
            )])),
        );
        assert!(errors_of(&create).is_empty(), "{create:?}");
        let pull = data(&create)
            .get("createPullRequest")
            .unwrap()
            .get("pullRequest")
            .unwrap();
        assert_eq!(pull.get("number").unwrap().as_i64(), Some(1));
        assert_eq!(pull.get("state").unwrap().as_str(), Some("OPEN"));
        assert_eq!(pull.get("headRefName").unwrap().as_str(), Some("feature"));
        assert_eq!(pull.get("baseRefName").unwrap().as_str(), Some("main"));
        let pull_id = pull.get("id").unwrap().as_str().unwrap().to_string();

        let files = gql(
            &mut store,
            "query PRFiles($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { pullRequest(number: 1) { files(first: 10) { nodes { path } } } } }",
            Some(crate::json::obj(vec![
                ("owner", Json::Str("adaiversen".to_string())),
                ("name", Json::Str("demo".to_string())),
            ])),
        );
        assert!(errors_of(&files).is_empty(), "{files:?}");
        let nodes = data(&files)
            .get("repository")
            .unwrap()
            .get("pullRequest")
            .unwrap()
            .get("files")
            .unwrap()
            .get("nodes")
            .unwrap()
            .as_arr()
            .unwrap();
        let paths: Vec<&str> = nodes
            .iter()
            .filter_map(|node| node.get("path"))
            .filter_map(|path| path.as_str())
            .collect();
        assert!(paths.contains(&"feature.txt"), "paths: {paths:?}");

        let merge = gql(
            &mut store,
            "mutation MergePR($input: MergePullRequestInput!) { mergePullRequest(input: $input) { pullRequest { number merged state } } }",
            Some(crate::json::obj(vec![(
                "input",
                crate::json::obj(vec![
                    ("pullRequestId", Json::Str(pull_id)),
                    ("mergeMethod", Json::Str("SQUASH".to_string())),
                ]),
            )])),
        );
        assert!(errors_of(&merge).is_empty(), "{merge:?}");
        let merged = data(&merge)
            .get("mergePullRequest")
            .unwrap()
            .get("pullRequest")
            .unwrap();
        assert_eq!(merged.get("merged").unwrap().as_bool(), Some(true));
        assert_eq!(merged.get("state").unwrap().as_str(), Some("MERGED"));
    }

    #[test]
    fn add_comment_reads_back() {
        let mut store = test_store();
        let repo_id = store
            .repo_gid(store.get_repo("adaiversen", "demo").unwrap())
            .clone();
        gql(
            &mut store,
            "mutation CreateIssue($input: CreateIssueInput!) { createIssue(input: $input) { issue { number } } }",
            Some(crate::json::obj(vec![(
                "input",
                crate::json::obj(vec![
                    ("repositoryId", Json::Str(repo_id.clone())),
                    ("title", Json::Str("Needs a comment".to_string())),
                ]),
            )])),
        );
        let issue_id = store
            .issue_gid(store.get_repo("adaiversen", "demo").unwrap(), 1)
            .clone();
        let result = gql(
            &mut store,
            "mutation AddComment($input: AddCommentInput!) { addComment(input: $input) { commentEdge { node { id body } } } }",
            Some(crate::json::obj(vec![(
                "input",
                crate::json::obj(vec![
                    ("subjectId", Json::Str(issue_id)),
                    ("body", Json::Str("hello there".to_string())),
                ]),
            )])),
        );
        assert!(errors_of(&result).is_empty(), "{result:?}");
        let node = data(&result)
            .get("addComment")
            .unwrap()
            .get("commentEdge")
            .unwrap()
            .get("node")
            .unwrap();
        assert_eq!(node.get("body").unwrap().as_str(), Some("hello there"));
        assert!(!node.get("id").unwrap().as_str().unwrap().is_empty());
    }
}
