// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! SQL Parser

#[cfg(not(feature = "std"))]
use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::fmt;

use itertools::Itertools;
use tracing::{debug, instrument};

use crate::ast::*;
use crate::keywords::{self, Keyword};
use crate::parser_v2;
use crate::tokenizer::*;

pub(crate) const UPSTREAM_SOURCE_KEY: &str = "connector";

#[derive(Debug, Clone, PartialEq)]
pub enum ParserError {
    TokenizerError(String),
    ParserError(String),
}

impl ParserError {
    pub fn inner_msg(self) -> String {
        match self {
            ParserError::TokenizerError(s) | ParserError::ParserError(s) => s,
        }
    }
}
// Use `Parser::expected` instead, if possible
#[macro_export]
macro_rules! parser_err {
    ($MSG:expr) => {
        Err(ParserError::ParserError($MSG.to_string()))
    };
}

// Returns a successful result if the optional expression is some
macro_rules! return_ok_if_some {
    ($e:expr) => {{
        if let Some(v) = $e {
            return Ok(v);
        }
    }};
}

#[derive(PartialEq)]
pub enum IsOptional {
    Optional,
    Mandatory,
}

use IsOptional::*;

pub enum IsLateral {
    Lateral,
    NotLateral,
}

use IsLateral::*;

pub type IncludeOption = Vec<IncludeOptionItem>;

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Eq, Clone, Debug, PartialEq, Hash)]
pub struct IncludeOptionItem {
    pub column_type: Ident,
    pub column_alias: Option<Ident>,
    pub inner_field: Option<String>,
    pub header_inner_expect_type: Option<DataType>,
}

#[derive(Debug)]
pub enum WildcardOrExpr {
    Expr(Expr),
    /// Expr is an arbitrary expression, returning either a table or a column.
    /// Idents are the prefix of `*`, which are consecutive field accesses.
    /// e.g. `(table.v1).*` or `(table).v1.*`
    ///
    /// See also [`Expr::FieldIdentifier`] for behaviors of parentheses.
    ExprQualifiedWildcard(Expr, Vec<Ident>),
    /// `QualifiedWildcard` and `Wildcard` can be followed by EXCEPT (columns)
    QualifiedWildcard(ObjectName, Option<Vec<Expr>>),
    Wildcard(Option<Vec<Expr>>),
}

impl From<WildcardOrExpr> for FunctionArgExpr {
    fn from(wildcard_expr: WildcardOrExpr) -> Self {
        match wildcard_expr {
            WildcardOrExpr::Expr(expr) => Self::Expr(expr),
            WildcardOrExpr::ExprQualifiedWildcard(expr, prefix) => {
                Self::ExprQualifiedWildcard(expr, prefix)
            }
            WildcardOrExpr::QualifiedWildcard(prefix, except) => {
                Self::QualifiedWildcard(prefix, except)
            }
            WildcardOrExpr::Wildcard(except) => Self::Wildcard(except),
        }
    }
}

impl From<TokenizerError> for ParserError {
    fn from(e: TokenizerError) -> Self {
        ParserError::TokenizerError(e.to_string())
    }
}

impl fmt::Display for ParserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "sql parser error: {}",
            match self {
                ParserError::TokenizerError(s) => s,
                ParserError::ParserError(s) => s,
            }
        )
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ParserError {}

type ColumnsDefTuple = (
    Vec<ColumnDef>,
    Vec<TableConstraint>,
    Vec<SourceWatermark>,
    Option<usize>,
);

/// Reference:
/// <https://www.postgresql.org/docs/current/sql-syntax-lexical.html#SQL-PRECEDENCE>
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Precedence {
    Zero = 0,
    LogicalOr, // 5 in upstream
    LogicalXor,
    LogicalAnd, // 10 in upstream
    UnaryNot,   // 15 in upstream
    Is,         // 17 in upstream
    Cmp,
    Like,    // 19 in upstream
    Between, // 20 in upstream
    Other,
    PlusMinus, // 30 in upstream
    MulDiv,    // 40 in upstream
    Exp,
    UnaryPosNeg,
    PostfixFactorial,
    Array,
    DoubleColon, // 50 in upstream
}

pub struct Parser {
    tokens: Vec<TokenWithLocation>,
    /// The index of the first unprocessed token in `self.tokens`
    index: usize,
}

impl Parser {
    /// Parse the specified tokens
    pub fn new(tokens: Vec<TokenWithLocation>) -> Self {
        Parser { tokens, index: 0 }
    }

    /// Adaptor for [`parser_v2`].
    ///
    /// You can call a v2 parser from original parser by using this method.
    pub(crate) fn parse_v2<'a, O>(
        &'a mut self,
        mut parse_next: impl winnow::Parser<
            winnow::Located<parser_v2::TokenStreamWrapper<'a>>,
            O,
            winnow::error::ContextError,
        >,
    ) -> Result<O, ParserError> {
        use winnow::stream::Location;

        let mut token_stream = winnow::Located::new(parser_v2::TokenStreamWrapper {
            tokens: &self.tokens[self.index..],
        });
        let output = parse_next.parse_next(&mut token_stream).map_err(|e| {
            let msg = if let Some(e) = e.into_inner()
                && let Some(cause) = e.cause()
            {
                format!(": {}", cause)
            } else {
                "".to_string()
            };
            ParserError::ParserError(format!(
                "Unexpected {}{}",
                if self.index + token_stream.location() >= self.tokens.len() {
                    &"EOF" as &dyn std::fmt::Display
                } else {
                    &self.tokens[self.index + token_stream.location()] as &dyn std::fmt::Display
                },
                msg
            ))
        });
        let offset = token_stream.location();
        self.index += offset;
        output
    }

    /// Parse a SQL statement and produce an Abstract Syntax Tree (AST)
    #[instrument(level = "debug")]
    pub fn parse_sql(sql: &str) -> Result<Vec<Statement>, ParserError> {
        let mut tokenizer = Tokenizer::new(sql);
        let tokens = tokenizer.tokenize_with_location()?;
        let mut parser = Parser::new(tokens);
        let mut stmts = Vec::new();
        let mut expecting_statement_delimiter = false;
        loop {
            // ignore empty statements (between successive statement delimiters)
            while parser.consume_token(&Token::SemiColon) {
                expecting_statement_delimiter = false;
            }

            if parser.peek_token() == Token::EOF {
                break;
            }
            if expecting_statement_delimiter {
                return parser.expected("end of statement", parser.peek_token());
            }

            let statement = parser.parse_statement()?;
            stmts.push(statement);
            expecting_statement_delimiter = true;
        }
        debug!("parsed statements:\n{:#?}", stmts);
        Ok(stmts)
    }

    /// Parse a single top-level statement (such as SELECT, INSERT, CREATE, etc.),
    /// stopping before the statement separator, if any.
    pub fn parse_statement(&mut self) -> Result<Statement, ParserError> {
        let token = self.next_token();
        match token.token {
            Token::Word(w) => match w.keyword {
                Keyword::EXPLAIN => Ok(self.parse_explain()?),
                Keyword::ANALYZE => Ok(self.parse_analyze()?),
                Keyword::SELECT | Keyword::WITH | Keyword::VALUES => {
                    self.prev_token();
                    Ok(Statement::Query(Box::new(self.parse_query()?)))
                }
                Keyword::DECLARE => Ok(self.parse_declare()?),
                Keyword::FETCH => Ok(self.parse_fetch_cursor()?),
                Keyword::CLOSE => Ok(self.parse_close_cursor()?),
                Keyword::TRUNCATE => Ok(self.parse_truncate()?),
                Keyword::CREATE => Ok(self.parse_create()?),
                Keyword::DISCARD => Ok(self.parse_discard()?),
                Keyword::DROP => Ok(self.parse_drop()?),
                Keyword::DELETE => Ok(self.parse_delete()?),
                Keyword::INSERT => Ok(self.parse_insert()?),
                Keyword::UPDATE => Ok(self.parse_update()?),
                Keyword::ALTER => Ok(self.parse_alter()?),
                Keyword::COPY => Ok(self.parse_copy()?),
                Keyword::SET => Ok(self.parse_set()?),
                Keyword::SHOW => {
                    if self.parse_keyword(Keyword::CREATE) {
                        Ok(self.parse_show_create()?)
                    } else {
                        Ok(self.parse_show()?)
                    }
                }
                Keyword::CANCEL => Ok(self.parse_cancel_job()?),
                Keyword::KILL => Ok(self.parse_kill_process()?),
                Keyword::DESCRIBE => Ok(Statement::Describe {
                    name: self.parse_object_name()?,
                }),
                Keyword::GRANT => Ok(self.parse_grant()?),
                Keyword::REVOKE => Ok(self.parse_revoke()?),
                Keyword::START => Ok(self.parse_start_transaction()?),
                Keyword::ABORT => Ok(Statement::Abort),
                // `BEGIN` is a nonstandard but common alias for the
                // standard `START TRANSACTION` statement. It is supported
                // by at least PostgreSQL and MySQL.
                Keyword::BEGIN => Ok(self.parse_begin()?),
                Keyword::COMMIT => Ok(self.parse_commit()?),
                Keyword::ROLLBACK => Ok(self.parse_rollback()?),
                // `PREPARE`, `EXECUTE` and `DEALLOCATE` are Postgres-specific
                // syntaxes. They are used for Postgres prepared statement.
                Keyword::DEALLOCATE => Ok(self.parse_deallocate()?),
                Keyword::EXECUTE => Ok(self.parse_execute()?),
                Keyword::PREPARE => Ok(self.parse_prepare()?),
                Keyword::COMMENT => Ok(self.parse_comment()?),
                Keyword::FLUSH => Ok(Statement::Flush),
                Keyword::WAIT => Ok(Statement::Wait),
                Keyword::RECOVER => Ok(Statement::Recover),
                _ => self.expected(
                    "an SQL statement",
                    Token::Word(w).with_location(token.location),
                ),
            },
            Token::LParen => {
                self.prev_token();
                Ok(Statement::Query(Box::new(self.parse_query()?)))
            }
            unexpected => {
                self.expected("an SQL statement", unexpected.with_location(token.location))
            }
        }
    }

    pub fn parse_truncate(&mut self) -> Result<Statement, ParserError> {
        let _ = self.parse_keyword(Keyword::TABLE);
        let table_name = self.parse_object_name()?;
        Ok(Statement::Truncate { table_name })
    }

    pub fn parse_analyze(&mut self) -> Result<Statement, ParserError> {
        let table_name = self.parse_object_name()?;

        Ok(Statement::Analyze { table_name })
    }

    /// Tries to parse a wildcard expression. If it is not a wildcard, parses an expression.
    ///
    /// A wildcard expression either means:
    /// - Selecting all fields from a struct. In this case, it is a
    ///   [`WildcardOrExpr::ExprQualifiedWildcard`]. Similar to [`Expr::FieldIdentifier`], It must
    ///   contain parentheses.
    /// - Selecting all columns from a table. In this case, it is a
    ///   [`WildcardOrExpr::QualifiedWildcard`] or a [`WildcardOrExpr::Wildcard`].
    pub fn parse_wildcard_or_expr(&mut self) -> Result<WildcardOrExpr, ParserError> {
        let index = self.index;

        match self.next_token().token {
            Token::Word(w) if self.peek_token() == Token::Period => {
                // Since there's no parenthesis, `w` must be a column or a table
                // So what follows must be dot-delimited identifiers, e.g. `a.b.c.*`
                let wildcard_expr = self.parse_simple_wildcard_expr(index)?;
                return self.word_concat_wildcard_expr(w.to_ident()?, wildcard_expr);
            }
            Token::Mul => {
                return Ok(WildcardOrExpr::Wildcard(self.parse_except()?));
            }
            // parses wildcard field selection expression.
            // Code is similar to `parse_struct_selection`
            Token::LParen => {
                let mut expr = self.parse_expr()?;
                if self.consume_token(&Token::RParen) {
                    // Unwrap parentheses
                    while let Expr::Nested(inner) = expr {
                        expr = *inner;
                    }
                    // Now that we have an expr, what follows must be
                    // dot-delimited identifiers, e.g. `b.c.*` in `(a).b.c.*`
                    let wildcard_expr = self.parse_simple_wildcard_expr(index)?;
                    return self.expr_concat_wildcard_expr(expr, wildcard_expr);
                }
            }
            _ => (),
        };

        self.index = index;
        self.parse_expr().map(WildcardOrExpr::Expr)
    }

    /// Concats `ident` and `wildcard_expr` in `ident.wildcard_expr`
    pub fn word_concat_wildcard_expr(
        &mut self,
        ident: Ident,
        simple_wildcard_expr: WildcardOrExpr,
    ) -> Result<WildcardOrExpr, ParserError> {
        let mut idents = vec![ident];
        let mut except_cols = vec![];
        match simple_wildcard_expr {
            WildcardOrExpr::QualifiedWildcard(ids, except) => {
                idents.extend(ids.0);
                if let Some(cols) = except {
                    except_cols = cols;
                }
            }
            WildcardOrExpr::Wildcard(except) => {
                if let Some(cols) = except {
                    except_cols = cols;
                }
            }
            WildcardOrExpr::ExprQualifiedWildcard(_, _) => unreachable!(),
            WildcardOrExpr::Expr(e) => return Ok(WildcardOrExpr::Expr(e)),
        }
        Ok(WildcardOrExpr::QualifiedWildcard(
            ObjectName(idents),
            if except_cols.is_empty() {
                None
            } else {
                Some(except_cols)
            },
        ))
    }

    /// Concats `expr` and `wildcard_expr` in `(expr).wildcard_expr`.
    pub fn expr_concat_wildcard_expr(
        &mut self,
        expr: Expr,
        simple_wildcard_expr: WildcardOrExpr,
    ) -> Result<WildcardOrExpr, ParserError> {
        if let WildcardOrExpr::Expr(e) = simple_wildcard_expr {
            return Ok(WildcardOrExpr::Expr(e));
        }

        // similar to `parse_struct_selection`
        let mut idents = vec![];
        let expr = match expr {
            // expr is `(foo)`
            Expr::Identifier(_) => expr,
            // expr is `(foo.v1)`
            Expr::CompoundIdentifier(_) => expr,
            // expr is `((1,2,3)::foo)`
            Expr::Cast { .. } => expr,
            // expr is `(func())`
            Expr::Function(_) => expr,
            // expr is `((foo.v1).v2)`
            Expr::FieldIdentifier(expr, ids) => {
                // Put `ids` to the latter part!
                idents.extend(ids);
                *expr
            }
            // expr is other things, e.g., `(1+2)`. It will become an unexpected period error at
            // upper level.
            _ => return Ok(WildcardOrExpr::Expr(expr)),
        };

        match simple_wildcard_expr {
            WildcardOrExpr::QualifiedWildcard(ids, except) => {
                if except.is_some() {
                    return self.expected(
                        "Expr quantified wildcard does not support except",
                        self.peek_token(),
                    );
                }
                idents.extend(ids.0);
            }
            WildcardOrExpr::Wildcard(except) => {
                if except.is_some() {
                    return self.expected(
                        "Expr quantified wildcard does not support except",
                        self.peek_token(),
                    );
                }
            }
            WildcardOrExpr::ExprQualifiedWildcard(_, _) => unreachable!(),
            WildcardOrExpr::Expr(_) => unreachable!(),
        }
        Ok(WildcardOrExpr::ExprQualifiedWildcard(expr, idents))
    }

    /// Tries to parses a wildcard expression without any parentheses.
    ///
    /// If wildcard is not found, go back to `index` and parse an expression.
    pub fn parse_simple_wildcard_expr(
        &mut self,
        index: usize,
    ) -> Result<WildcardOrExpr, ParserError> {
        let mut id_parts = vec![];
        while self.consume_token(&Token::Period) {
            let token = self.next_token();
            match token.token {
                Token::Word(w) => id_parts.push(w.to_ident()?),
                Token::Mul => {
                    return if id_parts.is_empty() {
                        Ok(WildcardOrExpr::Wildcard(self.parse_except()?))
                    } else {
                        Ok(WildcardOrExpr::QualifiedWildcard(
                            ObjectName(id_parts),
                            self.parse_except()?,
                        ))
                    };
                }
                unexpected => {
                    return self.expected(
                        "an identifier or a '*' after '.'",
                        unexpected.with_location(token.location),
                    );
                }
            }
        }
        self.index = index;
        self.parse_expr().map(WildcardOrExpr::Expr)
    }

    pub fn parse_except(&mut self) -> Result<Option<Vec<Expr>>, ParserError> {
        if !self.parse_keyword(Keyword::EXCEPT) {
            return Ok(None);
        }
        if !self.consume_token(&Token::LParen) {
            return self.expected("EXCEPT should be followed by (", self.peek_token());
        }
        let exprs = self.parse_comma_separated(Parser::parse_expr)?;
        if self.consume_token(&Token::RParen) {
            Ok(Some(exprs))
        } else {
            self.expected(
                "( should be followed by ) after column names",
                self.peek_token(),
            )
        }
    }

    /// Parse a new expression
    pub fn parse_expr(&mut self) -> Result<Expr, ParserError> {
        self.parse_subexpr(Precedence::Zero)
    }

    /// Parse tokens until the precedence changes
    pub fn parse_subexpr(&mut self, precedence: Precedence) -> Result<Expr, ParserError> {
        debug!("parsing expr, current token: {:?}", self.peek_token().token);
        let mut expr = self.parse_prefix()?;
        debug!("prefix: {:?}", expr);
        loop {
            let next_precedence = self.get_next_precedence()?;
            debug!("precedence: {precedence:?}, next precedence: {next_precedence:?}");

            if precedence >= next_precedence {
                break;
            }

            expr = self.parse_infix(expr, next_precedence)?;
        }
        Ok(expr)
    }

    /// Parse an expression prefix
    pub fn parse_prefix(&mut self) -> Result<Expr, ParserError> {
        // PostgreSQL allows any string literal to be preceded by a type name, indicating that the
        // string literal represents a literal of that type. Some examples:
        //
        //      DATE '2020-05-20'
        //      TIMESTAMP WITH TIME ZONE '2020-05-20 7:43:54'
        //      BOOL 'true'
        //
        // The first two are standard SQL, while the latter is a PostgreSQL extension. Complicating
        // matters is the fact that INTERVAL string literals may optionally be followed by special
        // keywords, e.g.:
        //
        //      INTERVAL '7' DAY
        //
        // Note also that naively `SELECT date` looks like a syntax error because the `date` type
        // name is not followed by a string literal, but in fact in PostgreSQL it is a valid
        // expression that should parse as the column name "date".
        return_ok_if_some!(self.maybe_parse(|parser| {
            match parser.parse_data_type()? {
                DataType::Interval => parser.parse_literal_interval(),
                // PostgreSQL allows almost any identifier to be used as custom data type name,
                // and we support that in `parse_data_type()`. But unlike Postgres we don't
                // have a list of globally reserved keywords (since they vary across dialects),
                // so given `NOT 'a' LIKE 'b'`, we'd accept `NOT` as a possible custom data type
                // name, resulting in `NOT 'a'` being recognized as a `TypedString` instead of
                // an unary negation `NOT ('a' LIKE 'b')`. To solve this, we don't accept the
                // `type 'string'` syntax for the custom data types at all.
                DataType::Custom(..) => parser_err!("dummy"),
                data_type => Ok(Expr::TypedString {
                    data_type,
                    value: parser.parse_literal_string()?,
                }),
            }
        }));

        let token = self.next_token();
        let expr = match token.token.clone() {
            Token::Word(w) => match w.keyword {
                Keyword::TRUE | Keyword::FALSE | Keyword::NULL => {
                    self.prev_token();
                    Ok(Expr::Value(self.parse_value()?))
                }
                Keyword::CASE => self.parse_case_expr(),
                Keyword::CAST => self.parse_cast_expr(),
                Keyword::TRY_CAST => self.parse_try_cast_expr(),
                Keyword::EXISTS => self.parse_exists_expr(),
                Keyword::EXTRACT => self.parse_extract_expr(),
                Keyword::SUBSTRING => self.parse_substring_expr(),
                Keyword::POSITION => self.parse_position_expr(),
                Keyword::OVERLAY => self.parse_overlay_expr(),
                Keyword::TRIM => self.parse_trim_expr(),
                Keyword::INTERVAL => self.parse_literal_interval(),
                Keyword::NOT => Ok(Expr::UnaryOp {
                    op: UnaryOperator::Not,
                    expr: Box::new(self.parse_subexpr(Precedence::UnaryNot)?),
                }),
                Keyword::ROW => self.parse_row_expr(),
                Keyword::ARRAY if self.peek_token() == Token::LParen => {
                    // similar to `exists(subquery)`
                    self.expect_token(&Token::LParen)?;
                    let exists_node = Expr::ArraySubquery(Box::new(self.parse_query()?));
                    self.expect_token(&Token::RParen)?;
                    Ok(exists_node)
                }
                Keyword::ARRAY if self.peek_token() == Token::LBracket => self.parse_array_expr(),
                // `LEFT` and `RIGHT` are reserved as identifier but okay as function
                Keyword::LEFT | Keyword::RIGHT => {
                    self.parse_function(ObjectName(vec![w.to_ident()?]))
                }
                Keyword::OPERATOR if self.peek_token().token == Token::LParen => {
                    let op = UnaryOperator::PGQualified(Box::new(self.parse_qualified_operator()?));
                    Ok(Expr::UnaryOp {
                        op,
                        expr: Box::new(self.parse_subexpr(Precedence::Other)?),
                    })
                }
                keyword @ (Keyword::ALL | Keyword::ANY | Keyword::SOME) => {
                    self.expect_token(&Token::LParen)?;
                    // In upstream's PR of parser-rs, there is `self.parser_subexpr(precedence)` here.
                    // But it will fail to parse `select 1 = any(null and true);`.
                    let sub = self.parse_expr()?;
                    self.expect_token(&Token::RParen)?;

                    // TODO: support `all/any/some(subquery)`.
                    if let Expr::Subquery(_) = &sub {
                        parser_err!("ANY/SOME/ALL(Subquery) is not implemented")?;
                    }

                    Ok(match keyword {
                        Keyword::ALL => Expr::AllOp(Box::new(sub)),
                        // `SOME` is a synonym for `ANY`.
                        Keyword::ANY | Keyword::SOME => Expr::SomeOp(Box::new(sub)),
                        _ => unreachable!(),
                    })
                }
                k if keywords::RESERVED_FOR_COLUMN_OR_TABLE_NAME.contains(&k) => {
                    parser_err!(format!("syntax error at or near {token}"))
                }
                // Here `w` is a word, check if it's a part of a multi-part
                // identifier, a function call, or a simple identifier:
                _ => match self.peek_token().token {
                    Token::LParen | Token::Period => {
                        let mut id_parts: Vec<Ident> = vec![w.to_ident()?];
                        while self.consume_token(&Token::Period) {
                            let token = self.next_token();
                            match token.token {
                                Token::Word(w) => id_parts.push(w.to_ident()?),
                                unexpected => {
                                    return self.expected(
                                        "an identifier or a '*' after '.'",
                                        unexpected.with_location(token.location),
                                    );
                                }
                            }
                        }

                        if self.consume_token(&Token::LParen) {
                            self.prev_token();
                            self.parse_function(ObjectName(id_parts))
                        } else {
                            Ok(Expr::CompoundIdentifier(id_parts))
                        }
                    }
                    _ => Ok(Expr::Identifier(w.to_ident()?)),
                },
            }, // End of Token::Word

            tok @ Token::Minus | tok @ Token::Plus => {
                let op = if tok == Token::Plus {
                    UnaryOperator::Plus
                } else {
                    UnaryOperator::Minus
                };
                let mut sub_expr = self.parse_subexpr(Precedence::UnaryPosNeg)?;
                if let Expr::Value(Value::Number(ref mut s)) = sub_expr {
                    if tok == Token::Minus {
                        *s = format!("-{}", s);
                    }
                    return Ok(sub_expr);
                }
                Ok(Expr::UnaryOp {
                    op,
                    expr: Box::new(sub_expr),
                })
            }
            tok @ Token::DoubleExclamationMark
            | tok @ Token::PGSquareRoot
            | tok @ Token::PGCubeRoot
            | tok @ Token::AtSign
            | tok @ Token::Tilde => {
                let op = match tok {
                    Token::DoubleExclamationMark => UnaryOperator::PGPrefixFactorial,
                    Token::PGSquareRoot => UnaryOperator::PGSquareRoot,
                    Token::PGCubeRoot => UnaryOperator::PGCubeRoot,
                    Token::AtSign => UnaryOperator::PGAbs,
                    Token::Tilde => UnaryOperator::PGBitwiseNot,
                    _ => unreachable!(),
                };
                // Counter-intuitively, `|/ 4 + 12` means `|/ (4+12)` rather than `(|/4) + 12` in
                // PostgreSQL.
                Ok(Expr::UnaryOp {
                    op,
                    expr: Box::new(self.parse_subexpr(Precedence::Other)?),
                })
            }
            Token::Number(_)
            | Token::SingleQuotedString(_)
            | Token::DollarQuotedString(_)
            | Token::NationalStringLiteral(_)
            | Token::HexStringLiteral(_)
            | Token::CstyleEscapesString(_) => {
                self.prev_token();
                Ok(Expr::Value(self.parse_value()?))
            }
            Token::Parameter(number) => self.parse_param(number),
            Token::Pipe => {
                let args = self.parse_comma_separated(Parser::parse_identifier)?;
                self.expect_token(&Token::Pipe)?;
                let body = self.parse_expr()?;
                Ok(Expr::LambdaFunction {
                    args,
                    body: Box::new(body),
                })
            }
            Token::LParen => {
                let expr =
                    if self.parse_keyword(Keyword::SELECT) || self.parse_keyword(Keyword::WITH) {
                        self.prev_token();
                        Expr::Subquery(Box::new(self.parse_query()?))
                    } else {
                        let mut exprs = self.parse_comma_separated(Parser::parse_expr)?;
                        if exprs.len() == 1 {
                            Expr::Nested(Box::new(exprs.pop().unwrap()))
                        } else {
                            Expr::Row(exprs)
                        }
                    };
                self.expect_token(&Token::RParen)?;
                if self.peek_token() == Token::Period && matches!(expr, Expr::Nested(_)) {
                    self.parse_struct_selection(expr)
                } else {
                    Ok(expr)
                }
            }
            unexpected => self.expected("an expression:", unexpected.with_location(token.location)),
        }?;

        if self.parse_keyword(Keyword::COLLATE) {
            Ok(Expr::Collate {
                expr: Box::new(expr),
                collation: self.parse_object_name()?,
            })
        } else {
            Ok(expr)
        }
    }

    fn parse_param(&mut self, param: String) -> Result<Expr, ParserError> {
        Ok(Expr::Parameter {
            index: param.parse().map_err(|_| {
                ParserError::ParserError(format!("Parameter symbol has a invalid index {}.", param))
            })?,
        })
    }

    /// Parses a field selection expression. See also [`Expr::FieldIdentifier`].
    pub fn parse_struct_selection(&mut self, expr: Expr) -> Result<Expr, ParserError> {
        let mut nested_expr = expr;
        // Unwrap parentheses
        while let Expr::Nested(inner) = nested_expr {
            nested_expr = *inner;
        }
        let fields = self.parse_fields()?;
        Ok(Expr::FieldIdentifier(Box::new(nested_expr), fields))
    }

    /// Parses consecutive field identifiers after a period. i.e., `.foo.bar.baz`
    pub fn parse_fields(&mut self) -> Result<Vec<Ident>, ParserError> {
        let mut idents = vec![];
        while self.consume_token(&Token::Period) {
            let token = self.next_token();
            match token.token {
                Token::Word(w) => {
                    idents.push(w.to_ident()?);
                }
                unexpected => {
                    return self.expected(
                        "an identifier after '.'",
                        unexpected.with_location(token.location),
                    );
                }
            }
        }
        Ok(idents)
    }

    pub fn parse_qualified_operator(&mut self) -> Result<QualifiedOperator, ParserError> {
        self.expect_token(&Token::LParen)?;

        let schema = match self.parse_identifier_non_reserved() {
            Ok(ident) => {
                self.expect_token(&Token::Period)?;
                Some(ident)
            }
            Err(_) => {
                self.prev_token();
                None
            }
        };

        // https://www.postgresql.org/docs/15/sql-syntax-lexical.html#SQL-SYNTAX-OPERATORS
        const OP_CHARS: &[char] = &[
            '+', '-', '*', '/', '<', '>', '=', '~', '!', '@', '#', '%', '^', '&', '|', '`', '?',
        ];
        let name = {
            // Unlike PostgreSQL, we only take 1 token here rather than any sequence of `OP_CHARS`.
            // This is enough because we do not support custom operators like `x *@ y` anyways,
            // and all builtin sequences are already single tokens.
            //
            // To support custom operators and be fully compatible with PostgreSQL later, the
            // tokenizer should also be updated.
            let token = self.next_token();
            let name = token.token.to_string();
            if !name.trim_matches(OP_CHARS).is_empty() {
                self.prev_token();
                return self.expected(&format!("one of {}", OP_CHARS.iter().join(" ")), token);
            }
            name
        };

        self.expect_token(&Token::RParen)?;
        Ok(QualifiedOperator { schema, name })
    }

    pub fn parse_function(&mut self, name: ObjectName) -> Result<Expr, ParserError> {
        self.expect_token(&Token::LParen)?;
        let distinct = self.parse_all_or_distinct()?;
        let (args, order_by, variadic) = self.parse_optional_args()?;
        let over = if self.parse_keyword(Keyword::OVER) {
            // TBD: support window names (`OVER mywin`) in place of inline specification
            self.expect_token(&Token::LParen)?;
            let partition_by = if self.parse_keywords(&[Keyword::PARTITION, Keyword::BY]) {
                // a list of possibly-qualified column names
                self.parse_comma_separated(Parser::parse_expr)?
            } else {
                vec![]
            };
            let order_by_window = if self.parse_keywords(&[Keyword::ORDER, Keyword::BY]) {
                self.parse_comma_separated(Parser::parse_order_by_expr)?
            } else {
                vec![]
            };
            let window_frame = if !self.consume_token(&Token::RParen) {
                let window_frame = self.parse_window_frame()?;
                self.expect_token(&Token::RParen)?;
                Some(window_frame)
            } else {
                None
            };

            Some(WindowSpec {
                partition_by,
                order_by: order_by_window,
                window_frame,
            })
        } else {
            None
        };

        let filter = if self.parse_keyword(Keyword::FILTER) {
            self.expect_token(&Token::LParen)?;
            self.expect_keyword(Keyword::WHERE)?;
            let filter_expr = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            Some(Box::new(filter_expr))
        } else {
            None
        };

        let within_group = if self.parse_keywords(&[Keyword::WITHIN, Keyword::GROUP]) {
            self.expect_token(&Token::LParen)?;
            self.expect_keywords(&[Keyword::ORDER, Keyword::BY])?;
            let order_by_parsed = self.parse_comma_separated(Parser::parse_order_by_expr)?;
            let order_by = order_by_parsed.iter().exactly_one().map_err(|_| {
                ParserError::ParserError("only one arg in order by is expected here".to_string())
            })?;
            self.expect_token(&Token::RParen)?;
            Some(Box::new(order_by.clone()))
        } else {
            None
        };

        Ok(Expr::Function(Function {
            name,
            args,
            variadic,
            over,
            distinct,
            order_by,
            filter,
            within_group,
        }))
    }

    pub fn parse_window_frame_units(&mut self) -> Result<WindowFrameUnits, ParserError> {
        let token = self.next_token();
        match token.token {
            Token::Word(w) => match w.keyword {
                Keyword::ROWS => Ok(WindowFrameUnits::Rows),
                Keyword::RANGE => Ok(WindowFrameUnits::Range),
                Keyword::GROUPS => Ok(WindowFrameUnits::Groups),
                _ => self.expected(
                    "ROWS, RANGE, GROUPS",
                    Token::Word(w).with_location(token.location),
                )?,
            },
            unexpected => self.expected(
                "ROWS, RANGE, GROUPS",
                unexpected.with_location(token.location),
            ),
        }
    }

    pub fn parse_window_frame(&mut self) -> Result<WindowFrame, ParserError> {
        let units = self.parse_window_frame_units()?;
        let (start_bound, end_bound) = if self.parse_keyword(Keyword::BETWEEN) {
            let start_bound = self.parse_window_frame_bound()?;
            self.expect_keyword(Keyword::AND)?;
            let end_bound = Some(self.parse_window_frame_bound()?);
            (start_bound, end_bound)
        } else {
            (self.parse_window_frame_bound()?, None)
        };
        let exclusion = if self.parse_keyword(Keyword::EXCLUDE) {
            Some(self.parse_window_frame_exclusion()?)
        } else {
            None
        };
        Ok(WindowFrame {
            units,
            start_bound,
            end_bound,
            exclusion,
        })
    }

    /// Parse `CURRENT ROW` or `{ <non-negative numeric | datetime | interval> | UNBOUNDED } { PRECEDING | FOLLOWING }`
    pub fn parse_window_frame_bound(&mut self) -> Result<WindowFrameBound, ParserError> {
        if self.parse_keywords(&[Keyword::CURRENT, Keyword::ROW]) {
            Ok(WindowFrameBound::CurrentRow)
        } else {
            let rows = if self.parse_keyword(Keyword::UNBOUNDED) {
                None
            } else {
                Some(Box::new(self.parse_expr()?))
            };
            if self.parse_keyword(Keyword::PRECEDING) {
                Ok(WindowFrameBound::Preceding(rows))
            } else if self.parse_keyword(Keyword::FOLLOWING) {
                Ok(WindowFrameBound::Following(rows))
            } else {
                self.expected("PRECEDING or FOLLOWING", self.peek_token())
            }
        }
    }

    pub fn parse_window_frame_exclusion(&mut self) -> Result<WindowFrameExclusion, ParserError> {
        if self.parse_keywords(&[Keyword::CURRENT, Keyword::ROW]) {
            Ok(WindowFrameExclusion::CurrentRow)
        } else if self.parse_keyword(Keyword::GROUP) {
            Ok(WindowFrameExclusion::Group)
        } else if self.parse_keyword(Keyword::TIES) {
            Ok(WindowFrameExclusion::Ties)
        } else if self.parse_keywords(&[Keyword::NO, Keyword::OTHERS]) {
            Ok(WindowFrameExclusion::NoOthers)
        } else {
            self.expected("CURRENT ROW, GROUP, TIES, or NO OTHERS", self.peek_token())
        }
    }

    /// parse a group by expr. a group by expr can be one of group sets, roll up, cube, or simple
    /// expr.
    fn parse_group_by_expr(&mut self) -> Result<Expr, ParserError> {
        if self.parse_keywords(&[Keyword::GROUPING, Keyword::SETS]) {
            self.expect_token(&Token::LParen)?;
            let result = self.parse_comma_separated(|p| p.parse_tuple(true, true))?;
            self.expect_token(&Token::RParen)?;
            Ok(Expr::GroupingSets(result))
        } else if self.parse_keyword(Keyword::CUBE) {
            self.expect_token(&Token::LParen)?;
            let result = self.parse_comma_separated(|p| p.parse_tuple(true, false))?;
            self.expect_token(&Token::RParen)?;
            Ok(Expr::Cube(result))
        } else if self.parse_keyword(Keyword::ROLLUP) {
            self.expect_token(&Token::LParen)?;
            let result = self.parse_comma_separated(|p| p.parse_tuple(true, false))?;
            self.expect_token(&Token::RParen)?;
            Ok(Expr::Rollup(result))
        } else {
            self.parse_expr()
        }
    }

    /// parse a tuple with `(` and `)`.
    /// If `lift_singleton` is true, then a singleton tuple is lifted to a tuple of length 1,
    /// otherwise it will fail. If `allow_empty` is true, then an empty tuple is allowed.
    fn parse_tuple(
        &mut self,
        lift_singleton: bool,
        allow_empty: bool,
    ) -> Result<Vec<Expr>, ParserError> {
        if lift_singleton {
            if self.consume_token(&Token::LParen) {
                let result = if allow_empty && self.consume_token(&Token::RParen) {
                    vec![]
                } else {
                    let result = self.parse_comma_separated(Parser::parse_expr)?;
                    self.expect_token(&Token::RParen)?;
                    result
                };
                Ok(result)
            } else {
                Ok(vec![self.parse_expr()?])
            }
        } else {
            self.expect_token(&Token::LParen)?;
            let result = if allow_empty && self.consume_token(&Token::RParen) {
                vec![]
            } else {
                let result = self.parse_comma_separated(Parser::parse_expr)?;
                self.expect_token(&Token::RParen)?;
                result
            };
            Ok(result)
        }
    }

    pub fn parse_case_expr(&mut self) -> Result<Expr, ParserError> {
        let mut operand = None;
        if !self.parse_keyword(Keyword::WHEN) {
            operand = Some(Box::new(self.parse_expr()?));
            self.expect_keyword(Keyword::WHEN)?;
        }
        let mut conditions = vec![];
        let mut results = vec![];
        loop {
            conditions.push(self.parse_expr()?);
            self.expect_keyword(Keyword::THEN)?;
            results.push(self.parse_expr()?);
            if !self.parse_keyword(Keyword::WHEN) {
                break;
            }
        }
        let else_result = if self.parse_keyword(Keyword::ELSE) {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect_keyword(Keyword::END)?;
        Ok(Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        })
    }

    /// Parse a SQL CAST function e.g. `CAST(expr AS FLOAT)`
    pub fn parse_cast_expr(&mut self) -> Result<Expr, ParserError> {
        self.expect_token(&Token::LParen)?;
        let expr = self.parse_expr()?;
        self.expect_keyword(Keyword::AS)?;
        let data_type = self.parse_data_type()?;
        self.expect_token(&Token::RParen)?;
        Ok(Expr::Cast {
            expr: Box::new(expr),
            data_type,
        })
    }

    /// Parse a SQL TRY_CAST function e.g. `TRY_CAST(expr AS FLOAT)`
    pub fn parse_try_cast_expr(&mut self) -> Result<Expr, ParserError> {
        self.expect_token(&Token::LParen)?;
        let expr = self.parse_expr()?;
        self.expect_keyword(Keyword::AS)?;
        let data_type = self.parse_data_type()?;
        self.expect_token(&Token::RParen)?;
        Ok(Expr::TryCast {
            expr: Box::new(expr),
            data_type,
        })
    }

    /// Parse a SQL EXISTS expression e.g. `WHERE EXISTS(SELECT ...)`.
    pub fn parse_exists_expr(&mut self) -> Result<Expr, ParserError> {
        self.expect_token(&Token::LParen)?;
        let exists_node = Expr::Exists(Box::new(self.parse_query()?));
        self.expect_token(&Token::RParen)?;
        Ok(exists_node)
    }

    pub fn parse_extract_expr(&mut self) -> Result<Expr, ParserError> {
        self.expect_token(&Token::LParen)?;
        let field = self.parse_date_time_field_in_extract()?;
        self.expect_keyword(Keyword::FROM)?;
        let expr = self.parse_expr()?;
        self.expect_token(&Token::RParen)?;
        Ok(Expr::Extract {
            field,
            expr: Box::new(expr),
        })
    }

    pub fn parse_substring_expr(&mut self) -> Result<Expr, ParserError> {
        // PARSE SUBSTRING (EXPR [FROM 1] [FOR 3])
        self.expect_token(&Token::LParen)?;
        let expr = self.parse_expr()?;
        let mut from_expr = None;
        if self.parse_keyword(Keyword::FROM) || self.consume_token(&Token::Comma) {
            from_expr = Some(self.parse_expr()?);
        }

        let mut to_expr = None;
        if self.parse_keyword(Keyword::FOR) || self.consume_token(&Token::Comma) {
            to_expr = Some(self.parse_expr()?);
        }
        self.expect_token(&Token::RParen)?;

        Ok(Expr::Substring {
            expr: Box::new(expr),
            substring_from: from_expr.map(Box::new),
            substring_for: to_expr.map(Box::new),
        })
    }

    /// `POSITION(<expr> IN <expr>)`
    pub fn parse_position_expr(&mut self) -> Result<Expr, ParserError> {
        self.expect_token(&Token::LParen)?;

        // Logically `parse_expr`, but limited to those with precedence higher than `BETWEEN`/`IN`,
        // to avoid conflict with general IN operator, for example `position(a IN (b) IN (c))`.
        // https://github.com/postgres/postgres/blob/REL_15_2/src/backend/parser/gram.y#L16012
        let substring = self.parse_subexpr(Precedence::Between)?;
        self.expect_keyword(Keyword::IN)?;
        let string = self.parse_subexpr(Precedence::Between)?;

        self.expect_token(&Token::RParen)?;

        Ok(Expr::Position {
            substring: Box::new(substring),
            string: Box::new(string),
        })
    }

    /// `OVERLAY(<expr> PLACING <expr> FROM <expr> [ FOR <expr> ])`
    pub fn parse_overlay_expr(&mut self) -> Result<Expr, ParserError> {
        self.expect_token(&Token::LParen)?;

        let expr = self.parse_expr()?;

        self.expect_keyword(Keyword::PLACING)?;
        let new_substring = self.parse_expr()?;

        self.expect_keyword(Keyword::FROM)?;
        let start = self.parse_expr()?;

        let mut count = None;
        if self.parse_keyword(Keyword::FOR) {
            count = Some(self.parse_expr()?);
        }

        self.expect_token(&Token::RParen)?;

        Ok(Expr::Overlay {
            expr: Box::new(expr),
            new_substring: Box::new(new_substring),
            start: Box::new(start),
            count: count.map(Box::new),
        })
    }

    /// `TRIM ([WHERE] ['text'] FROM 'text')`\
    /// `TRIM ([WHERE] [FROM] 'text' [, 'text'])`
    pub fn parse_trim_expr(&mut self) -> Result<Expr, ParserError> {
        self.expect_token(&Token::LParen)?;
        let mut trim_where = None;
        if let Token::Word(word) = self.peek_token().token {
            if [Keyword::BOTH, Keyword::LEADING, Keyword::TRAILING]
                .iter()
                .any(|d| word.keyword == *d)
            {
                trim_where = Some(self.parse_trim_where()?);
            }
        }

        let (mut trim_what, expr) = if self.parse_keyword(Keyword::FROM) {
            (None, self.parse_expr()?)
        } else {
            let mut expr = self.parse_expr()?;
            if self.parse_keyword(Keyword::FROM) {
                let trim_what = std::mem::replace(&mut expr, self.parse_expr()?);
                (Some(Box::new(trim_what)), expr)
            } else {
                (None, expr)
            }
        };

        if trim_what.is_none() && self.consume_token(&Token::Comma) {
            trim_what = Some(Box::new(self.parse_expr()?));
        }
        self.expect_token(&Token::RParen)?;

        Ok(Expr::Trim {
            expr: Box::new(expr),
            trim_where,
            trim_what,
        })
    }

    pub fn parse_trim_where(&mut self) -> Result<TrimWhereField, ParserError> {
        let token = self.next_token();
        match token.token {
            Token::Word(w) => match w.keyword {
                Keyword::BOTH => Ok(TrimWhereField::Both),
                Keyword::LEADING => Ok(TrimWhereField::Leading),
                Keyword::TRAILING => Ok(TrimWhereField::Trailing),
                _ => self.expected(
                    "trim_where field",
                    Token::Word(w).with_location(token.location),
                )?,
            },
            unexpected => {
                self.expected("trim_where field", unexpected.with_location(token.location))
            }
        }
    }

    /// Parses an array expression `[ex1, ex2, ..]`
    pub fn parse_array_expr(&mut self) -> Result<Expr, ParserError> {
        let mut expected_depth = None;
        let exprs = self.parse_array_inner(0, &mut expected_depth)?;
        Ok(Expr::Array(Array {
            elem: exprs,
            // Top-level array is named.
            named: true,
        }))
    }

    fn parse_array_inner(
        &mut self,
        depth: usize,
        expected_depth: &mut Option<usize>,
    ) -> Result<Vec<Expr>, ParserError> {
        self.expect_token(&Token::LBracket)?;
        if let Some(expected_depth) = *expected_depth
            && depth > expected_depth
        {
            return self.expected("]", self.peek_token());
        }
        let exprs = if self.peek_token() == Token::LBracket {
            self.parse_comma_separated(|parser| {
                let exprs = parser.parse_array_inner(depth + 1, expected_depth)?;
                Ok(Expr::Array(Array {
                    elem: exprs,
                    named: false,
                }))
            })?
        } else {
            if let Some(expected_depth) = *expected_depth {
                if depth < expected_depth {
                    return self.expected("[", self.peek_token());
                }
            } else {
                *expected_depth = Some(depth);
            }
            if self.consume_token(&Token::RBracket) {
                return Ok(vec![]);
            }
            self.parse_comma_separated(Self::parse_expr)?
        };
        self.expect_token(&Token::RBracket)?;
        Ok(exprs)
    }

    // This function parses date/time fields for interval qualifiers.
    pub fn parse_date_time_field(&mut self) -> Result<DateTimeField, ParserError> {
        let token = self.next_token();
        match token.token {
            Token::Word(w) => match w.keyword {
                Keyword::YEAR => Ok(DateTimeField::Year),
                Keyword::MONTH => Ok(DateTimeField::Month),
                Keyword::DAY => Ok(DateTimeField::Day),
                Keyword::HOUR => Ok(DateTimeField::Hour),
                Keyword::MINUTE => Ok(DateTimeField::Minute),
                Keyword::SECOND => Ok(DateTimeField::Second),
                _ => self.expected(
                    "date/time field",
                    Token::Word(w).with_location(token.location),
                )?,
            },
            unexpected => {
                self.expected("date/time field", unexpected.with_location(token.location))
            }
        }
    }

    // This function parses date/time fields for the EXTRACT function-like operator. PostgreSQL
    // allows arbitrary inputs including invalid ones.
    //
    // ```
    //   select extract(day from null::date);
    //   select extract(invalid from null::date);
    //   select extract("invaLId" from null::date);
    //   select extract('invaLId' from null::date);
    // ```
    pub fn parse_date_time_field_in_extract(&mut self) -> Result<String, ParserError> {
        let token = self.next_token();
        match token.token {
            Token::Word(w) => Ok(w.value.to_uppercase()),
            Token::SingleQuotedString(s) => Ok(s.to_uppercase()),
            unexpected => {
                self.expected("date/time field", unexpected.with_location(token.location))
            }
        }
    }

    /// Parse an INTERVAL literal.
    ///
    /// Some syntactically valid intervals:
    ///
    ///   1. `INTERVAL '1' DAY`
    ///   2. `INTERVAL '1-1' YEAR TO MONTH`
    ///   3. `INTERVAL '1' SECOND`
    ///   4. `INTERVAL '1:1:1.1' HOUR (5) TO SECOND (5)`
    ///   5. `INTERVAL '1.1' SECOND (2, 2)`
    ///   6. `INTERVAL '1:1' HOUR (5) TO MINUTE (5)`
    ///
    /// Note that we do not currently attempt to parse the quoted value.
    pub fn parse_literal_interval(&mut self) -> Result<Expr, ParserError> {
        // The SQL standard allows an optional sign before the value string, but
        // it is not clear if any implementations support that syntax, so we
        // don't currently try to parse it. (The sign can instead be included
        // inside the value string.)

        // The first token in an interval is a string literal which specifies
        // the duration of the interval.
        let value = self.parse_literal_string()?;

        // Following the string literal is a qualifier which indicates the units
        // of the duration specified in the string literal.
        //
        // Note that PostgreSQL allows omitting the qualifier, so we provide
        // this more general implementation.
        let leading_field = match self.peek_token().token {
            Token::Word(kw)
                if [
                    Keyword::YEAR,
                    Keyword::MONTH,
                    Keyword::DAY,
                    Keyword::HOUR,
                    Keyword::MINUTE,
                    Keyword::SECOND,
                ]
                .iter()
                .any(|d| kw.keyword == *d) =>
            {
                Some(self.parse_date_time_field()?)
            }
            _ => None,
        };

        let (leading_precision, last_field, fsec_precision) =
            if leading_field == Some(DateTimeField::Second) {
                // SQL mandates special syntax for `SECOND TO SECOND` literals.
                // Instead of
                //     `SECOND [(<leading precision>)] TO SECOND[(<fractional seconds precision>)]`
                // one must use the special format:
                //     `SECOND [( <leading precision> [ , <fractional seconds precision>] )]`
                let last_field = None;
                let (leading_precision, fsec_precision) = self.parse_optional_precision_scale()?;
                (leading_precision, last_field, fsec_precision)
            } else {
                let leading_precision = self.parse_optional_precision()?;
                if self.parse_keyword(Keyword::TO) {
                    let last_field = Some(self.parse_date_time_field()?);
                    let fsec_precision = if last_field == Some(DateTimeField::Second) {
                        self.parse_optional_precision()?
                    } else {
                        None
                    };
                    (leading_precision, last_field, fsec_precision)
                } else {
                    (leading_precision, None, None)
                }
            };

        Ok(Expr::Value(Value::Interval {
            value,
            leading_field,
            leading_precision,
            last_field,
            fractional_seconds_precision: fsec_precision,
        }))
    }

    /// Parse an operator following an expression
    pub fn parse_infix(&mut self, expr: Expr, precedence: Precedence) -> Result<Expr, ParserError> {
        let tok = self.next_token();
        debug!("parsing infix {:?}", tok.token);
        let regular_binary_operator = match &tok.token {
            Token::Spaceship => Some(BinaryOperator::Spaceship),
            Token::DoubleEq => Some(BinaryOperator::Eq),
            Token::Eq => Some(BinaryOperator::Eq),
            Token::Neq => Some(BinaryOperator::NotEq),
            Token::Gt => Some(BinaryOperator::Gt),
            Token::GtEq => Some(BinaryOperator::GtEq),
            Token::Lt => Some(BinaryOperator::Lt),
            Token::LtEq => Some(BinaryOperator::LtEq),
            Token::Plus => Some(BinaryOperator::Plus),
            Token::Minus => Some(BinaryOperator::Minus),
            Token::Mul => Some(BinaryOperator::Multiply),
            Token::Mod => Some(BinaryOperator::Modulo),
            Token::Concat => Some(BinaryOperator::Concat),
            Token::Pipe => Some(BinaryOperator::BitwiseOr),
            Token::Caret => Some(BinaryOperator::BitwiseXor),
            Token::Prefix => Some(BinaryOperator::Prefix),
            Token::Ampersand => Some(BinaryOperator::BitwiseAnd),
            Token::Div => Some(BinaryOperator::Divide),
            Token::ShiftLeft => Some(BinaryOperator::PGBitwiseShiftLeft),
            Token::ShiftRight => Some(BinaryOperator::PGBitwiseShiftRight),
            Token::Sharp => Some(BinaryOperator::PGBitwiseXor),
            Token::Tilde => Some(BinaryOperator::PGRegexMatch),
            Token::TildeAsterisk => Some(BinaryOperator::PGRegexIMatch),
            Token::ExclamationMarkTilde => Some(BinaryOperator::PGRegexNotMatch),
            Token::ExclamationMarkTildeAsterisk => Some(BinaryOperator::PGRegexNotIMatch),
            Token::DoubleTilde => Some(BinaryOperator::PGLikeMatch),
            Token::DoubleTildeAsterisk => Some(BinaryOperator::PGILikeMatch),
            Token::ExclamationMarkDoubleTilde => Some(BinaryOperator::PGNotLikeMatch),
            Token::ExclamationMarkDoubleTildeAsterisk => Some(BinaryOperator::PGNotILikeMatch),
            Token::Arrow => Some(BinaryOperator::Arrow),
            Token::LongArrow => Some(BinaryOperator::LongArrow),
            Token::HashArrow => Some(BinaryOperator::HashArrow),
            Token::HashLongArrow => Some(BinaryOperator::HashLongArrow),
            Token::HashMinus => Some(BinaryOperator::HashMinus),
            Token::AtArrow => Some(BinaryOperator::Contains),
            Token::ArrowAt => Some(BinaryOperator::Contained),
            Token::QuestionMark => Some(BinaryOperator::Exists),
            Token::QuestionMarkPipe => Some(BinaryOperator::ExistsAny),
            Token::QuestionMarkAmpersand => Some(BinaryOperator::ExistsAll),
            Token::AtQuestionMark => Some(BinaryOperator::PathExists),
            Token::AtAt => Some(BinaryOperator::PathMatch),
            Token::Word(w) => match w.keyword {
                Keyword::AND => Some(BinaryOperator::And),
                Keyword::OR => Some(BinaryOperator::Or),
                Keyword::XOR => Some(BinaryOperator::Xor),
                Keyword::OPERATOR if self.peek_token() == Token::LParen => Some(
                    BinaryOperator::PGQualified(Box::new(self.parse_qualified_operator()?)),
                ),
                _ => None,
            },
            _ => None,
        };

        if let Some(op) = regular_binary_operator {
            // // `all/any/some` only appears to the right of the binary op.
            // if let Some(keyword) =
            //     self.parse_one_of_keywords(&[Keyword::ANY, Keyword::ALL, Keyword::SOME])
            // {
            //     self.expect_token(&Token::LParen)?;
            //     // In upstream's PR of parser-rs, there is `self.parser_subexpr(precedence)` here.
            //     // But it will fail to parse `select 1 = any(null and true);`.
            //     let right = self.parse_expr()?;
            //     self.expect_token(&Token::RParen)?;

            //     // TODO: support `all/any/some(subquery)`.
            //     if let Expr::Subquery(_) = &right {
            //         parser_err!("ANY/SOME/ALL(Subquery) is not implemented")?;
            //     }

            //     let right = match keyword {
            //         Keyword::ALL => Box::new(Expr::AllOp(Box::new(right))),
            //         // `SOME` is a synonym for `ANY`.
            //         Keyword::ANY | Keyword::SOME => Box::new(Expr::SomeOp(Box::new(right))),
            //         _ => unreachable!(),
            //     };

            //     Ok(Expr::BinaryOp {
            //         left: Box::new(expr),
            //         op,
            //         right,
            //     })
            // } else {
            Ok(Expr::BinaryOp {
                left: Box::new(expr),
                op,
                right: Box::new(self.parse_subexpr(precedence)?),
            })
            // }
        } else if let Token::Word(w) = &tok.token {
            match w.keyword {
                Keyword::IS => {
                    if self.parse_keyword(Keyword::TRUE) {
                        Ok(Expr::IsTrue(Box::new(expr)))
                    } else if self.parse_keywords(&[Keyword::NOT, Keyword::TRUE]) {
                        Ok(Expr::IsNotTrue(Box::new(expr)))
                    } else if self.parse_keyword(Keyword::FALSE) {
                        Ok(Expr::IsFalse(Box::new(expr)))
                    } else if self.parse_keywords(&[Keyword::NOT, Keyword::FALSE]) {
                        Ok(Expr::IsNotFalse(Box::new(expr)))
                    } else if self.parse_keyword(Keyword::UNKNOWN) {
                        Ok(Expr::IsUnknown(Box::new(expr)))
                    } else if self.parse_keywords(&[Keyword::NOT, Keyword::UNKNOWN]) {
                        Ok(Expr::IsNotUnknown(Box::new(expr)))
                    } else if self.parse_keyword(Keyword::NULL) {
                        Ok(Expr::IsNull(Box::new(expr)))
                    } else if self.parse_keywords(&[Keyword::NOT, Keyword::NULL]) {
                        Ok(Expr::IsNotNull(Box::new(expr)))
                    } else if self.parse_keywords(&[Keyword::DISTINCT, Keyword::FROM]) {
                        let expr2 = self.parse_expr()?;
                        Ok(Expr::IsDistinctFrom(Box::new(expr), Box::new(expr2)))
                    } else if self.parse_keywords(&[Keyword::NOT, Keyword::DISTINCT, Keyword::FROM])
                    {
                        let expr2 = self.parse_expr()?;
                        Ok(Expr::IsNotDistinctFrom(Box::new(expr), Box::new(expr2)))
                    } else {
                        let negated = self.parse_keyword(Keyword::NOT);

                        if self.parse_keyword(Keyword::JSON) {
                            self.parse_is_json(expr, negated)
                        } else {
                            self.expected(
                                "[NOT] { TRUE | FALSE | UNKNOWN | NULL | DISTINCT FROM | JSON } after IS",
                                self.peek_token(),
                            )
                        }
                    }
                }
                Keyword::AT => {
                    if self.parse_keywords(&[Keyword::TIME, Keyword::ZONE]) {
                        let token = self.next_token();
                        match token.token {
                            Token::SingleQuotedString(time_zone) => Ok(Expr::AtTimeZone {
                                timestamp: Box::new(expr),
                                time_zone,
                            }),
                            unexpected => self.expected(
                                "Expected Token::SingleQuotedString after AT TIME ZONE",
                                unexpected.with_location(token.location),
                            ),
                        }
                    } else {
                        self.expected("Expected Token::Word after AT", tok)
                    }
                }
                keyword @ (Keyword::ALL | Keyword::ANY | Keyword::SOME) => {
                    self.expect_token(&Token::LParen)?;
                    // In upstream's PR of parser-rs, there is `self.parser_subexpr(precedence)` here.
                    // But it will fail to parse `select 1 = any(null and true);`.
                    let sub = self.parse_expr()?;
                    self.expect_token(&Token::RParen)?;

                    // TODO: support `all/any/some(subquery)`.
                    if let Expr::Subquery(_) = &sub {
                        parser_err!("ANY/SOME/ALL(Subquery) is not implemented")?;
                    }

                    Ok(match keyword {
                        Keyword::ALL => Expr::AllOp(Box::new(sub)),
                        // `SOME` is a synonym for `ANY`.
                        Keyword::ANY | Keyword::SOME => Expr::SomeOp(Box::new(sub)),
                        _ => unreachable!(),
                    })
                }
                Keyword::NOT
                | Keyword::IN
                | Keyword::BETWEEN
                | Keyword::LIKE
                | Keyword::ILIKE
                | Keyword::SIMILAR => {
                    self.prev_token();
                    let negated = self.parse_keyword(Keyword::NOT);
                    if self.parse_keyword(Keyword::IN) {
                        self.parse_in(expr, negated)
                    } else if self.parse_keyword(Keyword::BETWEEN) {
                        self.parse_between(expr, negated)
                    } else if self.parse_keyword(Keyword::LIKE) {
                        Ok(Expr::Like {
                            negated,
                            expr: Box::new(expr),
                            pattern: Box::new(self.parse_subexpr(Precedence::Like)?),
                            escape_char: self.parse_escape()?,
                        })
                    } else if self.parse_keyword(Keyword::ILIKE) {
                        Ok(Expr::ILike {
                            negated,
                            expr: Box::new(expr),
                            pattern: Box::new(self.parse_subexpr(Precedence::Like)?),
                            escape_char: self.parse_escape()?,
                        })
                    } else if self.parse_keywords(&[Keyword::SIMILAR, Keyword::TO]) {
                        Ok(Expr::SimilarTo {
                            negated,
                            expr: Box::new(expr),
                            pattern: Box::new(self.parse_subexpr(Precedence::Like)?),
                            escape_char: self.parse_escape()?,
                        })
                    } else {
                        self.expected("IN, BETWEEN or SIMILAR TO after NOT", self.peek_token())
                    }
                }
                // Can only happen if `get_next_precedence` got out of sync with this function
                _ => parser_err!(format!("No infix parser for token {:?}", tok)),
            }
        } else if Token::DoubleColon == tok {
            self.parse_pg_cast(expr)
        } else if Token::ExclamationMark == tok {
            // PostgreSQL factorial operation
            Ok(Expr::UnaryOp {
                op: UnaryOperator::PGPostfixFactorial,
                expr: Box::new(expr),
            })
        } else if Token::LBracket == tok {
            self.parse_array_index(expr)
        } else {
            // Can only happen if `get_next_precedence` got out of sync with this function
            parser_err!(format!("No infix parser for token {:?}", tok))
        }
    }

    /// parse the ESCAPE CHAR portion of LIKE, ILIKE, and SIMILAR TO
    pub fn parse_escape(&mut self) -> Result<Option<EscapeChar>, ParserError> {
        if self.parse_keyword(Keyword::ESCAPE) {
            let s = self.parse_literal_string()?;
            let mut chs = s.chars();
            if let Some(ch) = chs.next() {
                if chs.next().is_some() {
                    parser_err!(format!(
                        "Escape string must be empty or one character, found {s:?}"
                    ))
                } else {
                    Ok(Some(EscapeChar::escape(ch)))
                }
            } else {
                Ok(Some(EscapeChar::empty()))
            }
        } else {
            Ok(None)
        }
    }

    /// We parse both `array[1,9][1]`, `array[1,9][1:2]`, `array[1,9][:2]`, `array[1,9][1:]` and
    /// `array[1,9][:]` in this function.
    pub fn parse_array_index(&mut self, expr: Expr) -> Result<Expr, ParserError> {
        let new_expr = match self.peek_token().token {
            Token::Colon => {
                // [:] or [:N]
                assert!(self.consume_token(&Token::Colon));
                let end = match self.peek_token().token {
                    Token::RBracket => None,
                    _ => {
                        let end_index = Box::new(self.parse_expr()?);
                        Some(end_index)
                    }
                };
                Expr::ArrayRangeIndex {
                    obj: Box::new(expr),
                    start: None,
                    end,
                }
            }
            _ => {
                // [N], [N:], [N:M]
                let index = Box::new(self.parse_expr()?);
                match self.peek_token().token {
                    Token::Colon => {
                        // [N:], [N:M]
                        assert!(self.consume_token(&Token::Colon));
                        match self.peek_token().token {
                            Token::RBracket => {
                                // [N:]
                                Expr::ArrayRangeIndex {
                                    obj: Box::new(expr),
                                    start: Some(index),
                                    end: None,
                                }
                            }
                            _ => {
                                // [N:M]
                                let end = Some(Box::new(self.parse_expr()?));
                                Expr::ArrayRangeIndex {
                                    obj: Box::new(expr),
                                    start: Some(index),
                                    end,
                                }
                            }
                        }
                    }
                    _ => {
                        // [N]
                        Expr::ArrayIndex {
                            obj: Box::new(expr),
                            index,
                        }
                    }
                }
            }
        };
        self.expect_token(&Token::RBracket)?;
        // recursively checking for more indices
        if self.consume_token(&Token::LBracket) {
            self.parse_array_index(new_expr)
        } else {
            Ok(new_expr)
        }
    }

    /// Parses the optional constraints following the `IS [NOT] JSON` predicate
    pub fn parse_is_json(&mut self, expr: Expr, negated: bool) -> Result<Expr, ParserError> {
        let item_type = match self.peek_token().token {
            Token::Word(w) => match w.keyword {
                Keyword::VALUE => Some(JsonPredicateType::Value),
                Keyword::ARRAY => Some(JsonPredicateType::Array),
                Keyword::OBJECT => Some(JsonPredicateType::Object),
                Keyword::SCALAR => Some(JsonPredicateType::Scalar),
                _ => None,
            },
            _ => None,
        };
        if item_type.is_some() {
            self.next_token();
        }
        let item_type = item_type.unwrap_or_default();

        let unique_keys = self.parse_one_of_keywords(&[Keyword::WITH, Keyword::WITHOUT]);
        if unique_keys.is_some() {
            self.expect_keyword(Keyword::UNIQUE)?;
            _ = self.parse_keyword(Keyword::KEYS);
        }
        let unique_keys = unique_keys.is_some_and(|w| w == Keyword::WITH);

        Ok(Expr::IsJson {
            expr: Box::new(expr),
            negated,
            item_type,
            unique_keys,
        })
    }

    /// Parses the parens following the `[ NOT ] IN` operator
    pub fn parse_in(&mut self, expr: Expr, negated: bool) -> Result<Expr, ParserError> {
        self.expect_token(&Token::LParen)?;
        let in_op = if self.parse_keyword(Keyword::SELECT) || self.parse_keyword(Keyword::WITH) {
            self.prev_token();
            Expr::InSubquery {
                expr: Box::new(expr),
                subquery: Box::new(self.parse_query()?),
                negated,
            }
        } else {
            Expr::InList {
                expr: Box::new(expr),
                list: self.parse_comma_separated(Parser::parse_expr)?,
                negated,
            }
        };
        self.expect_token(&Token::RParen)?;
        Ok(in_op)
    }

    /// Parses `BETWEEN <low> AND <high>`, assuming the `BETWEEN` keyword was already consumed
    pub fn parse_between(&mut self, expr: Expr, negated: bool) -> Result<Expr, ParserError> {
        // Stop parsing subexpressions for <low> and <high> on tokens with
        // precedence lower than that of `BETWEEN`, such as `AND`, `IS`, etc.
        let low = self.parse_subexpr(Precedence::Between)?;
        self.expect_keyword(Keyword::AND)?;
        let high = self.parse_subexpr(Precedence::Between)?;
        Ok(Expr::Between {
            expr: Box::new(expr),
            negated,
            low: Box::new(low),
            high: Box::new(high),
        })
    }

    /// Parse a postgresql casting style which is in the form of `expr::datatype`
    pub fn parse_pg_cast(&mut self, expr: Expr) -> Result<Expr, ParserError> {
        Ok(Expr::Cast {
            expr: Box::new(expr),
            data_type: self.parse_data_type()?,
        })
    }

    /// Get the precedence of the next token
    pub fn get_next_precedence(&self) -> Result<Precedence, ParserError> {
        use Precedence as P;

        let token = self.peek_token();
        debug!("get_next_precedence() {:?}", token);
        match token.token {
            Token::Word(w) if w.keyword == Keyword::OR => Ok(P::LogicalOr),
            Token::Word(w) if w.keyword == Keyword::XOR => Ok(P::LogicalXor),
            Token::Word(w) if w.keyword == Keyword::AND => Ok(P::LogicalAnd),
            Token::Word(w) if w.keyword == Keyword::AT => {
                match (self.peek_nth_token(1).token, self.peek_nth_token(2).token) {
                    (Token::Word(w), Token::Word(w2))
                        if w.keyword == Keyword::TIME && w2.keyword == Keyword::ZONE =>
                    {
                        Ok(P::Other)
                    }
                    _ => Ok(P::Zero),
                }
            }

            Token::Word(w) if w.keyword == Keyword::NOT => match self.peek_nth_token(1).token {
                // The precedence of NOT varies depending on keyword that
                // follows it. If it is followed by IN, BETWEEN, or LIKE,
                // it takes on the precedence of those tokens. Otherwise it
                // is not an infix operator, and therefore has zero
                // precedence.
                Token::Word(w) if w.keyword == Keyword::BETWEEN => Ok(P::Between),
                Token::Word(w) if w.keyword == Keyword::IN => Ok(P::Between),
                Token::Word(w) if w.keyword == Keyword::LIKE => Ok(P::Like),
                Token::Word(w) if w.keyword == Keyword::ILIKE => Ok(P::Like),
                Token::Word(w) if w.keyword == Keyword::SIMILAR => Ok(P::Like),
                _ => Ok(P::Zero),
            },

            Token::Word(w) if w.keyword == Keyword::IS => Ok(P::Is),
            Token::Word(w) if w.keyword == Keyword::ISNULL => Ok(P::Is),
            Token::Word(w) if w.keyword == Keyword::NOTNULL => Ok(P::Is),
            Token::Eq
            | Token::Lt
            | Token::LtEq
            | Token::Neq
            | Token::Gt
            | Token::GtEq
            | Token::DoubleEq
            | Token::Spaceship => Ok(P::Cmp),
            Token::Word(w) if w.keyword == Keyword::IN => Ok(P::Between),
            Token::Word(w) if w.keyword == Keyword::BETWEEN => Ok(P::Between),
            Token::Word(w) if w.keyword == Keyword::LIKE => Ok(P::Like),
            Token::Word(w) if w.keyword == Keyword::ILIKE => Ok(P::Like),
            Token::Word(w) if w.keyword == Keyword::SIMILAR => Ok(P::Like),
            Token::Word(w) if w.keyword == Keyword::ALL => Ok(P::Other),
            Token::Word(w) if w.keyword == Keyword::ANY => Ok(P::Other),
            Token::Word(w) if w.keyword == Keyword::SOME => Ok(P::Other),
            Token::Tilde
            | Token::TildeAsterisk
            | Token::ExclamationMarkTilde
            | Token::ExclamationMarkTildeAsterisk
            | Token::DoubleTilde
            | Token::DoubleTildeAsterisk
            | Token::ExclamationMarkDoubleTilde
            | Token::ExclamationMarkDoubleTildeAsterisk
            | Token::Concat
            | Token::Prefix
            | Token::Arrow
            | Token::LongArrow
            | Token::HashArrow
            | Token::HashLongArrow
            | Token::HashMinus
            | Token::AtArrow
            | Token::ArrowAt
            | Token::QuestionMark
            | Token::QuestionMarkPipe
            | Token::QuestionMarkAmpersand
            | Token::AtQuestionMark
            | Token::AtAt => Ok(P::Other),
            Token::Word(w)
                if w.keyword == Keyword::OPERATOR && self.peek_nth_token(1) == Token::LParen =>
            {
                Ok(P::Other)
            }
            // In some languages (incl. rust, c), bitwise operators have precedence:
            //   or < xor < and < shift
            // But in PostgreSQL, they are just left to right. So `2 | 3 & 4` is 0.
            Token::Pipe => Ok(P::Other),
            Token::Sharp => Ok(P::Other),
            Token::Ampersand => Ok(P::Other),
            Token::ShiftRight | Token::ShiftLeft => Ok(P::Other),
            Token::Plus | Token::Minus => Ok(P::PlusMinus),
            Token::Mul | Token::Div | Token::Mod => Ok(P::MulDiv),
            Token::Caret => Ok(P::Exp),
            Token::ExclamationMark => Ok(P::PostfixFactorial),
            Token::LBracket => Ok(P::Array),
            Token::DoubleColon => Ok(P::DoubleColon),
            _ => Ok(P::Zero),
        }
    }

    /// Return the first non-whitespace token that has not yet been processed
    /// (or None if reached end-of-file)
    pub fn peek_token(&self) -> TokenWithLocation {
        self.peek_nth_token(0)
    }

    /// Return nth non-whitespace token that has not yet been processed
    pub fn peek_nth_token(&self, mut n: usize) -> TokenWithLocation {
        let mut index = self.index;
        loop {
            index += 1;
            let token = self.tokens.get(index - 1);
            match token.map(|x| &x.token) {
                Some(Token::Whitespace(_)) => continue,
                _ => {
                    if n == 0 {
                        return token
                            .cloned()
                            .unwrap_or(TokenWithLocation::wrap(Token::EOF));
                    }
                    n -= 1;
                }
            }
        }
    }

    /// Return the first non-whitespace token that has not yet been processed
    /// (or None if reached end-of-file) and mark it as processed. OK to call
    /// repeatedly after reaching EOF.
    pub fn next_token(&mut self) -> TokenWithLocation {
        loop {
            self.index += 1;
            let token = self.tokens.get(self.index - 1);
            match token.map(|x| &x.token) {
                Some(Token::Whitespace(_)) => continue,
                _ => {
                    return token
                        .cloned()
                        .unwrap_or(TokenWithLocation::wrap(Token::EOF));
                }
            }
        }
    }

    /// Return the first unprocessed token, possibly whitespace.
    pub fn next_token_no_skip(&mut self) -> Option<&TokenWithLocation> {
        self.index += 1;
        self.tokens.get(self.index - 1)
    }

    /// Push back the last one non-whitespace token. Must be called after
    /// `next_token()`, otherwise might panic. OK to call after
    /// `next_token()` indicates an EOF.
    pub fn prev_token(&mut self) {
        loop {
            assert!(self.index > 0);
            self.index -= 1;
            if let Some(token) = self.tokens.get(self.index)
                && let Token::Whitespace(_) = token.token
            {
                continue;
            }
            return;
        }
    }

    /// Report unexpected token
    pub fn expected<T>(&self, expected: &str, found: TokenWithLocation) -> Result<T, ParserError> {
        let start_off = self.index.saturating_sub(10);
        let end_off = self.index.min(self.tokens.len());
        let near_tokens = &self.tokens[start_off..end_off];
        struct TokensDisplay<'a>(&'a [TokenWithLocation]);
        impl<'a> fmt::Display for TokensDisplay<'a> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for token in self.0 {
                    write!(f, "{}", token.token)?;
                }
                Ok(())
            }
        }
        parser_err!(format!(
            "Expected {}, found: {}\nNear \"{}\"",
            expected,
            found,
            TokensDisplay(near_tokens),
        ))
    }

    /// Look for an expected keyword and consume it if it exists
    #[must_use]
    pub fn parse_keyword(&mut self, expected: Keyword) -> bool {
        match self.peek_token().token {
            Token::Word(w) if expected == w.keyword => {
                self.next_token();
                true
            }
            _ => false,
        }
    }

    /// Look for an expected sequence of keywords and consume them if they exist
    #[must_use]
    pub fn parse_keywords(&mut self, keywords: &[Keyword]) -> bool {
        let index = self.index;
        for &keyword in keywords {
            if !self.parse_keyword(keyword) {
                // println!("parse_keywords aborting .. did not find {:?}", keyword);
                // reset index and return immediately
                self.index = index;
                return false;
            }
        }
        true
    }

    /// Look for one of the given keywords and return the one that matches.
    #[must_use]
    pub fn parse_one_of_keywords(&mut self, keywords: &[Keyword]) -> Option<Keyword> {
        match self.peek_token().token {
            Token::Word(w) => {
                keywords
                    .iter()
                    .find(|keyword| **keyword == w.keyword)
                    .map(|keyword| {
                        self.next_token();
                        *keyword
                    })
            }
            _ => None,
        }
    }

    pub fn peek_nth_any_of_keywords(&mut self, n: usize, keywords: &[Keyword]) -> bool {
        match self.peek_nth_token(n).token {
            Token::Word(w) => keywords.iter().any(|keyword| *keyword == w.keyword),
            _ => false,
        }
    }

    /// Bail out if the current token is not one of the expected keywords, or consume it if it is
    pub fn expect_one_of_keywords(&mut self, keywords: &[Keyword]) -> Result<Keyword, ParserError> {
        if let Some(keyword) = self.parse_one_of_keywords(keywords) {
            Ok(keyword)
        } else {
            let keywords: Vec<String> = keywords.iter().map(|x| format!("{:?}", x)).collect();
            self.expected(
                &format!("one of {}", keywords.join(" or ")),
                self.peek_token(),
            )
        }
    }

    /// Bail out if the current token is not an expected keyword, or consume it if it is
    pub fn expect_keyword(&mut self, expected: Keyword) -> Result<(), ParserError> {
        if self.parse_keyword(expected) {
            Ok(())
        } else {
            self.expected(format!("{:?}", &expected).as_str(), self.peek_token())
        }
    }

    /// Bail out if the following tokens are not the expected sequence of
    /// keywords, or consume them if they are.
    pub fn expect_keywords(&mut self, expected: &[Keyword]) -> Result<(), ParserError> {
        for &kw in expected {
            self.expect_keyword(kw)?;
        }
        Ok(())
    }

    /// Consume the next token if it matches the expected token, otherwise return false
    #[must_use]
    pub fn consume_token(&mut self, expected: &Token) -> bool {
        if self.peek_token() == *expected {
            self.next_token();
            true
        } else {
            false
        }
    }

    /// Bail out if the current token is not an expected keyword, or consume it if it is
    pub fn expect_token(&mut self, expected: &Token) -> Result<(), ParserError> {
        if self.consume_token(expected) {
            Ok(())
        } else {
            self.expected(&expected.to_string(), self.peek_token())
        }
    }

    /// Parse a comma-separated list of 1+ items accepted by `F`
    pub fn parse_comma_separated<T, F>(&mut self, mut f: F) -> Result<Vec<T>, ParserError>
    where
        F: FnMut(&mut Parser) -> Result<T, ParserError>,
    {
        let mut values = vec![];
        loop {
            values.push(f(self)?);
            if !self.consume_token(&Token::Comma) {
                break;
            }
        }
        Ok(values)
    }

    /// Run a parser method `f`, reverting back to the current position
    /// if unsuccessful.
    #[must_use]
    fn maybe_parse<T, F>(&mut self, mut f: F) -> Option<T>
    where
        F: FnMut(&mut Parser) -> Result<T, ParserError>,
    {
        let index = self.index;
        if let Ok(t) = f(self) {
            Some(t)
        } else {
            self.index = index;
            None
        }
    }

    /// Parse either `ALL` or `DISTINCT`. Returns `true` if `DISTINCT` is parsed and results in a
    /// `ParserError` if both `ALL` and `DISTINCT` are fround.
    pub fn parse_all_or_distinct(&mut self) -> Result<bool, ParserError> {
        let all = self.parse_keyword(Keyword::ALL);
        let distinct = self.parse_keyword(Keyword::DISTINCT);
        if all && distinct {
            parser_err!("Cannot specify both ALL and DISTINCT".to_string())
        } else {
            Ok(distinct)
        }
    }

    /// Parse either `ALL` or `DISTINCT` or `DISTINCT ON (<expr>)`.
    pub fn parse_all_or_distinct_on(&mut self) -> Result<Distinct, ParserError> {
        if self.parse_keywords(&[Keyword::DISTINCT, Keyword::ON]) {
            self.expect_token(&Token::LParen)?;
            let exprs = self.parse_comma_separated(Parser::parse_expr)?;
            self.expect_token(&Token::RParen)?;
            return Ok(Distinct::DistinctOn(exprs));
        } else if self.parse_keyword(Keyword::DISTINCT) {
            return Ok(Distinct::Distinct);
        };
        _ = self.parse_keyword(Keyword::ALL);
        Ok(Distinct::All)
    }

    /// Parse a SQL CREATE statement
    pub fn parse_create(&mut self) -> Result<Statement, ParserError> {
        let or_replace = self.parse_keywords(&[Keyword::OR, Keyword::REPLACE]);
        let temporary = self
            .parse_one_of_keywords(&[Keyword::TEMP, Keyword::TEMPORARY])
            .is_some();
        if self.parse_keyword(Keyword::TABLE) {
            self.parse_create_table(or_replace, temporary)
        } else if self.parse_keyword(Keyword::VIEW) {
            self.parse_create_view(false, or_replace)
        } else if self.parse_keywords(&[Keyword::MATERIALIZED, Keyword::VIEW]) {
            self.parse_create_view(true, or_replace)
        } else if self.parse_keywords(&[Keyword::MATERIALIZED, Keyword::SOURCE]) {
            parser_err!("CREATE MATERIALIZED SOURCE has been deprecated, use CREATE TABLE instead")
        } else if self.parse_keyword(Keyword::SOURCE) {
            self.parse_create_source(or_replace)
        } else if self.parse_keyword(Keyword::SINK) {
            self.parse_create_sink(or_replace)
        } else if self.parse_keyword(Keyword::SUBSCRIPTION) {
            self.parse_create_subscription(or_replace)
        } else if self.parse_keyword(Keyword::CONNECTION) {
            self.parse_create_connection()
        } else if self.parse_keyword(Keyword::FUNCTION) {
            self.parse_create_function(or_replace, temporary)
        } else if self.parse_keyword(Keyword::AGGREGATE) {
            self.parse_create_aggregate(or_replace)
        } else if or_replace {
            self.expected(
                "[EXTERNAL] TABLE or [MATERIALIZED] VIEW or [MATERIALIZED] SOURCE or SINK or FUNCTION after CREATE OR REPLACE",
                self.peek_token(),
            )
        } else if self.parse_keyword(Keyword::INDEX) {
            self.parse_create_index(false)
        } else if self.parse_keywords(&[Keyword::UNIQUE, Keyword::INDEX]) {
            self.parse_create_index(true)
        } else if self.parse_keyword(Keyword::SCHEMA) {
            self.parse_create_schema()
        } else if self.parse_keyword(Keyword::DATABASE) {
            self.parse_create_database()
        } else if self.parse_keyword(Keyword::USER) {
            self.parse_create_user()
        } else {
            self.expected("an object type after CREATE", self.peek_token())
        }
    }

    pub fn parse_create_schema(&mut self) -> Result<Statement, ParserError> {
        let if_not_exists = self.parse_keywords(&[Keyword::IF, Keyword::NOT, Keyword::EXISTS]);
        let (schema_name, user_specified) = if self.parse_keyword(Keyword::AUTHORIZATION) {
            let user_specified = self.parse_object_name()?;
            (user_specified.clone(), Some(user_specified))
        } else {
            let schema_name = self.parse_object_name()?;
            let user_specified = if self.parse_keyword(Keyword::AUTHORIZATION) {
                Some(self.parse_object_name()?)
            } else {
                None
            };
            (schema_name, user_specified)
        };
        Ok(Statement::CreateSchema {
            schema_name,
            if_not_exists,
            user_specified,
        })
    }

    pub fn parse_create_database(&mut self) -> Result<Statement, ParserError> {
        let if_not_exists = self.parse_keywords(&[Keyword::IF, Keyword::NOT, Keyword::EXISTS]);
        let db_name = self.parse_object_name()?;
        Ok(Statement::CreateDatabase {
            db_name,
            if_not_exists,
        })
    }

    pub fn parse_create_view(
        &mut self,
        materialized: bool,
        or_replace: bool,
    ) -> Result<Statement, ParserError> {
        let if_not_exists = self.parse_keywords(&[Keyword::IF, Keyword::NOT, Keyword::EXISTS]);
        // Many dialects support `OR ALTER` right after `CREATE`, but we don't (yet).
        // ANSI SQL and Postgres support RECURSIVE here, but we don't support it either.
        let name = self.parse_object_name()?;
        let columns = self.parse_parenthesized_column_list(Optional)?;
        let with_options = self.parse_options_with_preceding_keyword(Keyword::WITH)?;
        self.expect_keyword(Keyword::AS)?;
        let query = Box::new(self.parse_query()?);
        let emit_mode = if materialized {
            self.parse_emit_mode()?
        } else {
            None
        };
        // Optional `WITH [ CASCADED | LOCAL ] CHECK OPTION` is widely supported here.
        Ok(Statement::CreateView {
            if_not_exists,
            name,
            columns,
            query,
            materialized,
            or_replace,
            with_options,
            emit_mode,
        })
    }

    // CREATE [OR REPLACE]?
    // [MATERIALIZED] SOURCE
    // [IF NOT EXISTS]?
    // <source_name: Ident>
    // [COLUMNS]?
    // [WITH (properties)]?
    // ROW FORMAT <row_format: Ident>
    // [ROW SCHEMA LOCATION <row_schema_location: String>]?
    pub fn parse_create_source(&mut self, _or_replace: bool) -> Result<Statement, ParserError> {
        Ok(Statement::CreateSource {
            stmt: CreateSourceStatement::parse_to(self)?,
        })
    }

    // CREATE [OR REPLACE]?
    // SINK
    // [IF NOT EXISTS]?
    // <sink_name: Ident>
    // FROM
    // <materialized_view: Ident>
    // [WITH (properties)]?
    pub fn parse_create_sink(&mut self, _or_replace: bool) -> Result<Statement, ParserError> {
        Ok(Statement::CreateSink {
            stmt: CreateSinkStatement::parse_to(self)?,
        })
    }

    // CREATE
    // SUBSCRIPTION
    // [IF NOT EXISTS]?
    // <subscription_name: Ident>
    // FROM
    // <materialized_view: Ident>
    // [WITH (properties)]?
    pub fn parse_create_subscription(
        &mut self,
        _or_replace: bool,
    ) -> Result<Statement, ParserError> {
        Ok(Statement::CreateSubscription {
            stmt: CreateSubscriptionStatement::parse_to(self)?,
        })
    }

    // CREATE
    // CONNECTION
    // [IF NOT EXISTS]?
    // <connection_name: Ident>
    // [WITH (properties)]?
    pub fn parse_create_connection(&mut self) -> Result<Statement, ParserError> {
        Ok(Statement::CreateConnection {
            stmt: CreateConnectionStatement::parse_to(self)?,
        })
    }

    pub fn parse_create_function(
        &mut self,
        or_replace: bool,
        temporary: bool,
    ) -> Result<Statement, ParserError> {
        let name = self.parse_object_name()?;
        self.expect_token(&Token::LParen)?;
        let args = if self.consume_token(&Token::RParen) {
            self.prev_token();
            None
        } else {
            Some(self.parse_comma_separated(Parser::parse_function_arg)?)
        };

        self.expect_token(&Token::RParen)?;

        let return_type = if self.parse_keyword(Keyword::RETURNS) {
            if self.parse_keyword(Keyword::TABLE) {
                self.expect_token(&Token::LParen)?;
                let mut values = vec![];
                loop {
                    values.push(self.parse_table_column_def()?);
                    let comma = self.consume_token(&Token::Comma);
                    if self.consume_token(&Token::RParen) {
                        // allow a trailing comma, even though it's not in standard
                        break;
                    } else if !comma {
                        return self.expected("',' or ')'", self.peek_token());
                    }
                }
                Some(CreateFunctionReturns::Table(values))
            } else {
                Some(CreateFunctionReturns::Value(self.parse_data_type()?))
            }
        } else {
            None
        };

        let params = self.parse_create_function_body()?;
        let with_options = self.parse_options_with_preceding_keyword(Keyword::WITH)?;
        let with_options = with_options.try_into()?;
        Ok(Statement::CreateFunction {
            or_replace,
            temporary,
            name,
            args,
            returns: return_type,
            params,
            with_options,
        })
    }

    fn parse_create_aggregate(&mut self, or_replace: bool) -> Result<Statement, ParserError> {
        let name = self.parse_object_name()?;
        self.expect_token(&Token::LParen)?;
        let args = self.parse_comma_separated(Parser::parse_function_arg)?;
        self.expect_token(&Token::RParen)?;

        self.expect_keyword(Keyword::RETURNS)?;
        let returns = self.parse_data_type()?;

        let append_only = self.parse_keywords(&[Keyword::APPEND, Keyword::ONLY]);
        let params = self.parse_create_function_body()?;

        Ok(Statement::CreateAggregate {
            or_replace,
            name,
            args,
            returns,
            append_only,
            params,
        })
    }

    pub fn parse_declare(&mut self) -> Result<Statement, ParserError> {
        Ok(Statement::DeclareCursor {
            stmt: DeclareCursorStatement::parse_to(self)?,
        })
    }

    pub fn parse_fetch_cursor(&mut self) -> Result<Statement, ParserError> {
        Ok(Statement::FetchCursor {
            stmt: FetchCursorStatement::parse_to(self)?,
        })
    }

    pub fn parse_close_cursor(&mut self) -> Result<Statement, ParserError> {
        Ok(Statement::CloseCursor {
            stmt: CloseCursorStatement::parse_to(self)?,
        })
    }

    fn parse_table_column_def(&mut self) -> Result<TableColumnDef, ParserError> {
        Ok(TableColumnDef {
            name: self.parse_identifier_non_reserved()?,
            data_type: self.parse_data_type()?,
        })
    }

    fn parse_function_arg(&mut self) -> Result<OperateFunctionArg, ParserError> {
        let mode = if self.parse_keyword(Keyword::IN) {
            Some(ArgMode::In)
        } else if self.parse_keyword(Keyword::OUT) {
            Some(ArgMode::Out)
        } else if self.parse_keyword(Keyword::INOUT) {
            Some(ArgMode::InOut)
        } else {
            None
        };

        // parse: [ argname ] argtype
        let mut name = None;
        let mut data_type = self.parse_data_type()?;
        if let DataType::Custom(n) = &data_type
            && !matches!(self.peek_token().token, Token::Comma | Token::RParen)
        {
            // the first token is actually a name
            name = Some(n.0[0].clone());
            data_type = self.parse_data_type()?;
        }

        let default_expr = if self.parse_keyword(Keyword::DEFAULT) || self.consume_token(&Token::Eq)
        {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(OperateFunctionArg {
            mode,
            name,
            data_type,
            default_expr,
        })
    }

    fn parse_create_function_body(&mut self) -> Result<CreateFunctionBody, ParserError> {
        let mut body = CreateFunctionBody::default();
        loop {
            fn ensure_not_set<T>(field: &Option<T>, name: &str) -> Result<(), ParserError> {
                if field.is_some() {
                    return Err(ParserError::ParserError(format!(
                        "{name} specified more than once",
                    )));
                }
                Ok(())
            }
            if self.parse_keyword(Keyword::AS) {
                ensure_not_set(&body.as_, "AS")?;
                body.as_ = Some(self.parse_function_definition()?);
            } else if self.parse_keyword(Keyword::LANGUAGE) {
                ensure_not_set(&body.language, "LANGUAGE")?;
                body.language = Some(self.parse_identifier()?);
            } else if self.parse_keyword(Keyword::RUNTIME) {
                ensure_not_set(&body.runtime, "RUNTIME")?;
                body.runtime = Some(self.parse_identifier()?);
            } else if self.parse_keyword(Keyword::IMMUTABLE) {
                ensure_not_set(&body.behavior, "IMMUTABLE | STABLE | VOLATILE")?;
                body.behavior = Some(FunctionBehavior::Immutable);
            } else if self.parse_keyword(Keyword::STABLE) {
                ensure_not_set(&body.behavior, "IMMUTABLE | STABLE | VOLATILE")?;
                body.behavior = Some(FunctionBehavior::Stable);
            } else if self.parse_keyword(Keyword::VOLATILE) {
                ensure_not_set(&body.behavior, "IMMUTABLE | STABLE | VOLATILE")?;
                body.behavior = Some(FunctionBehavior::Volatile);
            } else if self.parse_keyword(Keyword::RETURN) {
                ensure_not_set(&body.return_, "RETURN")?;
                body.return_ = Some(self.parse_expr()?);
            } else if self.parse_keyword(Keyword::USING) {
                ensure_not_set(&body.using, "USING")?;
                body.using = Some(self.parse_create_function_using()?);
            } else if self.parse_keyword(Keyword::SYNC) {
                ensure_not_set(&body.function_type, "SYNC | ASYNC")?;
                body.function_type = Some(self.parse_function_type(false, false)?);
            } else if self.parse_keyword(Keyword::ASYNC) {
                ensure_not_set(&body.function_type, "SYNC | ASYNC")?;
                body.function_type = Some(self.parse_function_type(true, false)?);
            } else if self.parse_keyword(Keyword::GENERATOR) {
                ensure_not_set(&body.function_type, "SYNC | ASYNC")?;
                body.function_type = Some(self.parse_function_type(false, true)?);
            } else {
                return Ok(body);
            }
        }
    }

    fn parse_create_function_using(&mut self) -> Result<CreateFunctionUsing, ParserError> {
        let keyword = self.expect_one_of_keywords(&[Keyword::LINK, Keyword::BASE64])?;

        match keyword {
            Keyword::LINK => {
                let uri = self.parse_literal_string()?;
                Ok(CreateFunctionUsing::Link(uri))
            }
            Keyword::BASE64 => {
                let base64 = self.parse_literal_string()?;
                Ok(CreateFunctionUsing::Base64(base64))
            }
            _ => unreachable!("{}", keyword),
        }
    }

    fn parse_function_type(
        &mut self,
        is_async: bool,
        is_generator: bool,
    ) -> Result<CreateFunctionType, ParserError> {
        let is_generator = if is_generator {
            true
        } else {
            self.parse_keyword(Keyword::GENERATOR)
        };

        match (is_async, is_generator) {
            (false, false) => Ok(CreateFunctionType::Sync),
            (true, false) => Ok(CreateFunctionType::Async),
            (false, true) => Ok(CreateFunctionType::Generator),
            (true, true) => Ok(CreateFunctionType::AsyncGenerator),
        }
    }

    // CREATE USER name [ [ WITH ] option [ ... ] ]
    // where option can be:
    //       SUPERUSER | NOSUPERUSER
    //     | CREATEDB | NOCREATEDB
    //     | CREATEUSER | NOCREATEUSER
    //     | LOGIN | NOLOGIN
    //     | [ ENCRYPTED ] PASSWORD 'password' | PASSWORD NULL | OAUTH
    fn parse_create_user(&mut self) -> Result<Statement, ParserError> {
        Ok(Statement::CreateUser(CreateUserStatement::parse_to(self)?))
    }

    pub fn parse_with_properties(&mut self) -> Result<Vec<SqlOption>, ParserError> {
        Ok(self
            .parse_options_with_preceding_keyword(Keyword::WITH)?
            .to_vec())
    }

    pub fn parse_discard(&mut self) -> Result<Statement, ParserError> {
        self.expect_keyword(Keyword::ALL)
            .map_err(|_| ParserError::ParserError("only DISCARD ALL is supported".to_string()))?;
        Ok(Statement::Discard(DiscardType::All))
    }

    pub fn parse_drop(&mut self) -> Result<Statement, ParserError> {
        if self.parse_keyword(Keyword::FUNCTION) {
            return self.parse_drop_function();
        } else if self.parse_keyword(Keyword::AGGREGATE) {
            return self.parse_drop_aggregate();
        }
        Ok(Statement::Drop(DropStatement::parse_to(self)?))
    }

    /// ```sql
    /// DROP FUNCTION [ IF EXISTS ] name [ ( [ [ argmode ] [ argname ] argtype [, ...] ] ) ] [, ...]
    /// [ CASCADE | RESTRICT ]
    /// ```
    fn parse_drop_function(&mut self) -> Result<Statement, ParserError> {
        let if_exists = self.parse_keywords(&[Keyword::IF, Keyword::EXISTS]);
        let func_desc = self.parse_comma_separated(Parser::parse_function_desc)?;
        let option = match self.parse_one_of_keywords(&[Keyword::CASCADE, Keyword::RESTRICT]) {
            Some(Keyword::CASCADE) => Some(ReferentialAction::Cascade),
            Some(Keyword::RESTRICT) => Some(ReferentialAction::Restrict),
            _ => None,
        };
        Ok(Statement::DropFunction {
            if_exists,
            func_desc,
            option,
        })
    }

    /// ```sql
    /// DROP AGGREGATE [ IF EXISTS ] name [ ( [ [ argmode ] [ argname ] argtype [, ...] ] ) ] [, ...]
    /// [ CASCADE | RESTRICT ]
    /// ```
    fn parse_drop_aggregate(&mut self) -> Result<Statement, ParserError> {
        let if_exists = self.parse_keywords(&[Keyword::IF, Keyword::EXISTS]);
        let func_desc = self.parse_comma_separated(Parser::parse_function_desc)?;
        let option = match self.parse_one_of_keywords(&[Keyword::CASCADE, Keyword::RESTRICT]) {
            Some(Keyword::CASCADE) => Some(ReferentialAction::Cascade),
            Some(Keyword::RESTRICT) => Some(ReferentialAction::Restrict),
            _ => None,
        };
        Ok(Statement::DropAggregate {
            if_exists,
            func_desc,
            option,
        })
    }

    fn parse_function_desc(&mut self) -> Result<FunctionDesc, ParserError> {
        let name = self.parse_object_name()?;

        let args = if self.consume_token(&Token::LParen) {
            if self.consume_token(&Token::RParen) {
                Some(vec![])
            } else {
                let args = self.parse_comma_separated(Parser::parse_function_arg)?;
                self.expect_token(&Token::RParen)?;
                Some(args)
            }
        } else {
            None
        };

        Ok(FunctionDesc { name, args })
    }

    pub fn parse_create_index(&mut self, unique: bool) -> Result<Statement, ParserError> {
        let if_not_exists = self.parse_keywords(&[Keyword::IF, Keyword::NOT, Keyword::EXISTS]);
        let index_name = self.parse_object_name()?;
        self.expect_keyword(Keyword::ON)?;
        let table_name = self.parse_object_name()?;
        self.expect_token(&Token::LParen)?;
        let columns = self.parse_comma_separated(Parser::parse_order_by_expr)?;
        self.expect_token(&Token::RParen)?;
        let mut include = vec![];
        if self.parse_keyword(Keyword::INCLUDE) {
            self.expect_token(&Token::LParen)?;
            include = self.parse_comma_separated(Parser::parse_identifier_non_reserved)?;
            self.expect_token(&Token::RParen)?;
        }
        let mut distributed_by = vec![];
        if self.parse_keywords(&[Keyword::DISTRIBUTED, Keyword::BY]) {
            self.expect_token(&Token::LParen)?;
            distributed_by = self.parse_comma_separated(Parser::parse_expr)?;
            self.expect_token(&Token::RParen)?;
        }
        Ok(Statement::CreateIndex {
            name: index_name,
            table_name,
            columns,
            include,
            distributed_by,
            unique,
            if_not_exists,
        })
    }

    pub fn parse_with_version_column(&mut self) -> Result<Option<String>, ParserError> {
        if self.parse_keywords(&[Keyword::WITH, Keyword::VERSION, Keyword::COLUMN]) {
            self.expect_token(&Token::LParen)?;
            let name = self.parse_identifier_non_reserved()?;
            self.expect_token(&Token::RParen)?;
            Ok(Some(name.value))
        } else {
            Ok(None)
        }
    }

    pub fn parse_on_conflict(&mut self) -> Result<Option<OnConflict>, ParserError> {
        if self.parse_keywords(&[Keyword::ON, Keyword::CONFLICT]) {
            self.parse_handle_conflict_behavior()
        } else {
            Ok(None)
        }
    }

    pub fn parse_create_table(
        &mut self,
        or_replace: bool,
        temporary: bool,
    ) -> Result<Statement, ParserError> {
        let if_not_exists = self.parse_keywords(&[Keyword::IF, Keyword::NOT, Keyword::EXISTS]);
        let table_name = self.parse_object_name()?;
        // parse optional column list (schema) and watermarks on source.
        let (columns, constraints, source_watermarks, wildcard_idx) =
            self.parse_columns_with_watermark()?;

        let append_only = if self.parse_keyword(Keyword::APPEND) {
            self.expect_keyword(Keyword::ONLY)?;
            true
        } else {
            false
        };

        let on_conflict = self.parse_on_conflict()?;

        let with_version_column = self.parse_with_version_column()?;
        let include_options = self.parse_include_options()?;

        // PostgreSQL supports `WITH ( options )`, before `AS`
        let with_options = self.parse_with_properties()?;

        let option = with_options
            .iter()
            .find(|&opt| opt.name.real_value() == UPSTREAM_SOURCE_KEY);
        let connector = option.map(|opt| opt.value.to_string());

        let source_schema = if let Some(connector) = connector {
            Some(self.parse_source_schema_with_connector(&connector, false)?)
        } else {
            None // Table is NOT created with an external connector.
        };
        // Parse optional `AS ( query )`
        let query = if self.parse_keyword(Keyword::AS) {
            if !source_watermarks.is_empty() {
                return Err(ParserError::ParserError(
                    "Watermarks can't be defined on table created by CREATE TABLE AS".to_string(),
                ));
            }
            Some(Box::new(self.parse_query()?))
        } else {
            None
        };

        let cdc_table_info = if self.parse_keyword(Keyword::FROM) {
            let source_name = self.parse_object_name()?;
            self.expect_keyword(Keyword::TABLE)?;
            let external_table_name = self.parse_literal_string()?;
            Some(CdcTableInfo {
                source_name,
                external_table_name,
            })
        } else {
            None
        };

        Ok(Statement::CreateTable {
            name: table_name,
            temporary,
            columns,
            wildcard_idx,
            constraints,
            with_options,
            or_replace,
            if_not_exists,
            source_schema,
            source_watermarks,
            append_only,
            on_conflict,
            with_version_column,
            query,
            cdc_table_info,
            include_column_options: include_options,
        })
    }

    pub fn parse_include_options(&mut self) -> Result<IncludeOption, ParserError> {
        let mut options = vec![];
        while self.parse_keyword(Keyword::INCLUDE) {
            let column_type = self.parse_identifier()?;

            let mut column_inner_field = None;
            let mut header_inner_expect_type = None;
            if let Token::SingleQuotedString(inner_field) = self.peek_token().token {
                self.next_token();
                column_inner_field = Some(inner_field);

                if let Token::Word(w) = self.peek_token().token {
                    match w.keyword {
                        Keyword::BYTEA => {
                            header_inner_expect_type = Some(DataType::Bytea);
                            self.next_token();
                        }
                        Keyword::VARCHAR => {
                            header_inner_expect_type = Some(DataType::Varchar);
                            self.next_token();
                        }
                        _ => {
                            // default to bytea
                            header_inner_expect_type = Some(DataType::Bytea);
                        }
                    }
                }
            }

            let mut column_alias = None;
            if self.parse_keyword(Keyword::AS) {
                column_alias = Some(self.parse_identifier()?);
            }

            options.push(IncludeOptionItem {
                column_type,
                inner_field: column_inner_field,
                column_alias,
                header_inner_expect_type,
            });
        }
        Ok(options)
    }

    pub fn parse_columns_with_watermark(&mut self) -> Result<ColumnsDefTuple, ParserError> {
        let mut columns = vec![];
        let mut constraints = vec![];
        let mut watermarks = vec![];
        let mut wildcard_idx = None;
        if !self.consume_token(&Token::LParen) || self.consume_token(&Token::RParen) {
            return Ok((columns, constraints, watermarks, wildcard_idx));
        }

        loop {
            if self.consume_token(&Token::Mul) {
                if wildcard_idx.is_none() {
                    wildcard_idx = Some(columns.len());
                } else {
                    return Err(ParserError::ParserError(
                        "At most 1 wildcard is allowed in source definetion".to_string(),
                    ));
                }
            } else if let Some(constraint) = self.parse_optional_table_constraint()? {
                constraints.push(constraint);
            } else if let Some(watermark) = self.parse_optional_watermark()? {
                watermarks.push(watermark);
                if watermarks.len() > 1 {
                    // TODO(yuhao): allow multiple watermark on source.
                    return Err(ParserError::ParserError(
                        "Only 1 watermark is allowed to be defined on source.".to_string(),
                    ));
                }
            } else if let Token::Word(_) = self.peek_token().token {
                columns.push(self.parse_column_def()?);
            } else {
                return self.expected("column name or constraint definition", self.peek_token());
            }
            let comma = self.consume_token(&Token::Comma);
            if self.consume_token(&Token::RParen) {
                // allow a trailing comma, even though it's not in standard
                break;
            } else if !comma {
                return self.expected("',' or ')' after column definition", self.peek_token());
            }
        }

        Ok((columns, constraints, watermarks, wildcard_idx))
    }

    fn parse_column_def(&mut self) -> Result<ColumnDef, ParserError> {
        let name = self.parse_identifier_non_reserved()?;
        let data_type = if let Token::Word(_) = self.peek_token().token {
            Some(self.parse_data_type()?)
        } else {
            None
        };

        let collation = if self.parse_keyword(Keyword::COLLATE) {
            Some(self.parse_object_name()?)
        } else {
            None
        };
        let mut options = vec![];
        loop {
            if self.parse_keyword(Keyword::CONSTRAINT) {
                let name = Some(self.parse_identifier_non_reserved()?);
                if let Some(option) = self.parse_optional_column_option()? {
                    options.push(ColumnOptionDef { name, option });
                } else {
                    return self.expected(
                        "constraint details after CONSTRAINT <name>",
                        self.peek_token(),
                    );
                }
            } else if let Some(option) = self.parse_optional_column_option()? {
                options.push(ColumnOptionDef { name: None, option });
            } else {
                break;
            };
        }
        Ok(ColumnDef {
            name,
            data_type,
            collation,
            options,
        })
    }

    pub fn parse_optional_column_option(&mut self) -> Result<Option<ColumnOption>, ParserError> {
        if self.parse_keywords(&[Keyword::NOT, Keyword::NULL]) {
            Ok(Some(ColumnOption::NotNull))
        } else if self.parse_keyword(Keyword::NULL) {
            Ok(Some(ColumnOption::Null))
        } else if self.parse_keyword(Keyword::DEFAULT) {
            Ok(Some(ColumnOption::DefaultColumns(self.parse_expr()?)))
        } else if self.parse_keywords(&[Keyword::PRIMARY, Keyword::KEY]) {
            Ok(Some(ColumnOption::Unique { is_primary: true }))
        } else if self.parse_keyword(Keyword::UNIQUE) {
            Ok(Some(ColumnOption::Unique { is_primary: false }))
        } else if self.parse_keyword(Keyword::REFERENCES) {
            let foreign_table = self.parse_object_name()?;
            // PostgreSQL allows omitting the column list and
            // uses the primary key column of the foreign table by default
            let referred_columns = self.parse_parenthesized_column_list(Optional)?;
            let mut on_delete = None;
            let mut on_update = None;
            loop {
                if on_delete.is_none() && self.parse_keywords(&[Keyword::ON, Keyword::DELETE]) {
                    on_delete = Some(self.parse_referential_action()?);
                } else if on_update.is_none()
                    && self.parse_keywords(&[Keyword::ON, Keyword::UPDATE])
                {
                    on_update = Some(self.parse_referential_action()?);
                } else {
                    break;
                }
            }
            Ok(Some(ColumnOption::ForeignKey {
                foreign_table,
                referred_columns,
                on_delete,
                on_update,
            }))
        } else if self.parse_keyword(Keyword::CHECK) {
            self.expect_token(&Token::LParen)?;
            let expr = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            Ok(Some(ColumnOption::Check(expr)))
        } else if self.parse_keyword(Keyword::AS) {
            Ok(Some(ColumnOption::GeneratedColumns(self.parse_expr()?)))
        } else {
            Ok(None)
        }
    }

    pub fn parse_handle_conflict_behavior(&mut self) -> Result<Option<OnConflict>, ParserError> {
        if self.parse_keyword(Keyword::OVERWRITE) {
            Ok(Some(OnConflict::OverWrite))
        } else if self.parse_keyword(Keyword::IGNORE) {
            Ok(Some(OnConflict::Ignore))
        } else if self.parse_keywords(&[
            Keyword::DO,
            Keyword::UPDATE,
            Keyword::IF,
            Keyword::NOT,
            Keyword::NULL,
        ]) {
            Ok(Some(OnConflict::DoUpdateIfNotNull))
        } else {
            Ok(None)
        }
    }

    pub fn parse_referential_action(&mut self) -> Result<ReferentialAction, ParserError> {
        if self.parse_keyword(Keyword::RESTRICT) {
            Ok(ReferentialAction::Restrict)
        } else if self.parse_keyword(Keyword::CASCADE) {
            Ok(ReferentialAction::Cascade)
        } else if self.parse_keywords(&[Keyword::SET, Keyword::NULL]) {
            Ok(ReferentialAction::SetNull)
        } else if self.parse_keywords(&[Keyword::NO, Keyword::ACTION]) {
            Ok(ReferentialAction::NoAction)
        } else if self.parse_keywords(&[Keyword::SET, Keyword::DEFAULT]) {
            Ok(ReferentialAction::SetDefault)
        } else {
            self.expected(
                "one of RESTRICT, CASCADE, SET NULL, NO ACTION or SET DEFAULT",
                self.peek_token(),
            )
        }
    }

    pub fn parse_optional_watermark(&mut self) -> Result<Option<SourceWatermark>, ParserError> {
        if self.parse_keyword(Keyword::WATERMARK) {
            self.expect_keyword(Keyword::FOR)?;
            let column = self.parse_identifier_non_reserved()?;
            self.expect_keyword(Keyword::AS)?;
            let expr = self.parse_expr()?;
            Ok(Some(SourceWatermark { column, expr }))
        } else {
            Ok(None)
        }
    }

    pub fn parse_optional_table_constraint(
        &mut self,
    ) -> Result<Option<TableConstraint>, ParserError> {
        let name = if self.parse_keyword(Keyword::CONSTRAINT) {
            Some(self.parse_identifier_non_reserved()?)
        } else {
            None
        };
        let token = self.next_token();
        match token.token {
            Token::Word(w) if w.keyword == Keyword::PRIMARY || w.keyword == Keyword::UNIQUE => {
                let is_primary = w.keyword == Keyword::PRIMARY;
                if is_primary {
                    self.expect_keyword(Keyword::KEY)?;
                }
                let columns = self.parse_parenthesized_column_list(Mandatory)?;
                Ok(Some(TableConstraint::Unique {
                    name,
                    columns,
                    is_primary,
                }))
            }
            Token::Word(w) if w.keyword == Keyword::FOREIGN => {
                self.expect_keyword(Keyword::KEY)?;
                let columns = self.parse_parenthesized_column_list(Mandatory)?;
                self.expect_keyword(Keyword::REFERENCES)?;
                let foreign_table = self.parse_object_name()?;
                let referred_columns = self.parse_parenthesized_column_list(Mandatory)?;
                let mut on_delete = None;
                let mut on_update = None;
                loop {
                    if on_delete.is_none() && self.parse_keywords(&[Keyword::ON, Keyword::DELETE]) {
                        on_delete = Some(self.parse_referential_action()?);
                    } else if on_update.is_none()
                        && self.parse_keywords(&[Keyword::ON, Keyword::UPDATE])
                    {
                        on_update = Some(self.parse_referential_action()?);
                    } else {
                        break;
                    }
                }
                Ok(Some(TableConstraint::ForeignKey {
                    name,
                    columns,
                    foreign_table,
                    referred_columns,
                    on_delete,
                    on_update,
                }))
            }
            Token::Word(w) if w.keyword == Keyword::CHECK => {
                self.expect_token(&Token::LParen)?;
                let expr = Box::new(self.parse_expr()?);
                self.expect_token(&Token::RParen)?;
                Ok(Some(TableConstraint::Check { name, expr }))
            }
            unexpected => {
                if name.is_some() {
                    self.expected(
                        "PRIMARY, UNIQUE, FOREIGN, or CHECK",
                        unexpected.with_location(token.location),
                    )
                } else {
                    self.prev_token();
                    Ok(None)
                }
            }
        }
    }

    pub fn parse_options_with_preceding_keyword(
        &mut self,
        keyword: Keyword,
    ) -> Result<Vec<SqlOption>, ParserError> {
        if self.parse_keyword(keyword) {
            self.expect_token(&Token::LParen)?;
            self.parse_options_inner()
        } else {
            Ok(vec![])
        }
    }

    pub fn parse_options(&mut self) -> Result<Vec<SqlOption>, ParserError> {
        if self.peek_token() == Token::LParen {
            self.next_token();
            self.parse_options_inner()
        } else {
            Ok(vec![])
        }
    }

    // has parsed a LParen
    pub fn parse_options_inner(&mut self) -> Result<Vec<SqlOption>, ParserError> {
        let mut values = vec![];
        loop {
            values.push(Parser::parse_sql_option(self)?);
            let comma = self.consume_token(&Token::Comma);
            if self.consume_token(&Token::RParen) {
                // allow a trailing comma, even though it's not in standard
                break;
            } else if !comma {
                return self.expected("',' or ')' after option definition", self.peek_token());
            }
        }
        Ok(values)
    }

    pub fn parse_sql_option(&mut self) -> Result<SqlOption, ParserError> {
        let name = self.parse_object_name()?;
        self.expect_token(&Token::Eq)?;
        let value = self.parse_value()?;
        Ok(SqlOption { name, value })
    }

    pub fn parse_since(&mut self) -> Result<Option<Since>, ParserError> {
        if self.parse_keyword(Keyword::SINCE) {
            let token = self.next_token();
            match token.token {
                Token::Word(w) => {
                    let ident = w.to_ident()?;
                    // Backward compatibility for now.
                    if ident.real_value() == "proctime" || ident.real_value() == "now" {
                        self.expect_token(&Token::LParen)?;
                        self.expect_token(&Token::RParen)?;
                        Ok(Some(Since::ProcessTime))
                    } else if ident.real_value() == "begin" {
                        self.expect_token(&Token::LParen)?;
                        self.expect_token(&Token::RParen)?;
                        Ok(Some(Since::Begin))
                    } else {
                        parser_err!(format!(
                            "Expected proctime(), begin() or now(), found: {}",
                            ident.real_value()
                        ))
                    }
                }
                Token::Number(s) => {
                    let num = s.parse::<u64>().map_err(|e| {
                        ParserError::ParserError(format!("Could not parse '{}' as u64: {}", s, e))
                    });
                    Ok(Some(Since::TimestampMsNum(num?)))
                }
                unexpected => self.expected(
                    "proctime(), begin() , now(), Number",
                    unexpected.with_location(token.location),
                ),
            }
        } else {
            Ok(None)
        }
    }

    pub fn parse_emit_mode(&mut self) -> Result<Option<EmitMode>, ParserError> {
        if self.parse_keyword(Keyword::EMIT) {
            match self.parse_one_of_keywords(&[Keyword::IMMEDIATELY, Keyword::ON]) {
                Some(Keyword::IMMEDIATELY) => Ok(Some(EmitMode::Immediately)),
                Some(Keyword::ON) => {
                    self.expect_keywords(&[Keyword::WINDOW, Keyword::CLOSE])?;
                    Ok(Some(EmitMode::OnWindowClose))
                }
                Some(_) => unreachable!(),
                None => self.expected(
                    "IMMEDIATELY or ON WINDOW CLOSE after EMIT",
                    self.peek_token(),
                ),
            }
        } else {
            Ok(None)
        }
    }

    pub fn parse_alter(&mut self) -> Result<Statement, ParserError> {
        if self.parse_keyword(Keyword::DATABASE) {
            self.parse_alter_database()
        } else if self.parse_keyword(Keyword::SCHEMA) {
            self.parse_alter_schema()
        } else if self.parse_keyword(Keyword::TABLE) {
            self.parse_alter_table()
        } else if self.parse_keyword(Keyword::INDEX) {
            self.parse_alter_index()
        } else if self.parse_keyword(Keyword::VIEW) {
            self.parse_alter_view(false)
        } else if self.parse_keywords(&[Keyword::MATERIALIZED, Keyword::VIEW]) {
            self.parse_alter_view(true)
        } else if self.parse_keyword(Keyword::SINK) {
            self.parse_alter_sink()
        } else if self.parse_keyword(Keyword::SOURCE) {
            self.parse_alter_source()
        } else if self.parse_keyword(Keyword::FUNCTION) {
            self.parse_alter_function()
        } else if self.parse_keyword(Keyword::CONNECTION) {
            self.parse_alter_connection()
        } else if self.parse_keyword(Keyword::USER) {
            self.parse_alter_user()
        } else if self.parse_keyword(Keyword::SYSTEM) {
            self.parse_alter_system()
        } else if self.parse_keyword(Keyword::SUBSCRIPTION) {
            self.parse_alter_subscription()
        } else {
            self.expected(
                "DATABASE, SCHEMA, TABLE, INDEX, MATERIALIZED, VIEW, SINK, SUBSCRIPTION, SOURCE, FUNCTION, USER or SYSTEM after ALTER",
                self.peek_token(),
            )
        }
    }

    pub fn parse_alter_database(&mut self) -> Result<Statement, ParserError> {
        let database_name = self.parse_object_name()?;
        let operation = if self.parse_keywords(&[Keyword::OWNER, Keyword::TO]) {
            let owner_name: Ident = self.parse_identifier()?;
            AlterDatabaseOperation::ChangeOwner {
                new_owner_name: owner_name,
            }
        } else if self.parse_keyword(Keyword::RENAME) {
            if self.parse_keyword(Keyword::TO) {
                let database_name = self.parse_object_name()?;
                AlterDatabaseOperation::RenameDatabase { database_name }
            } else {
                return self.expected("TO after RENAME", self.peek_token());
            }
        } else {
            return self.expected("OWNER TO after ALTER DATABASE", self.peek_token());
        };

        Ok(Statement::AlterDatabase {
            name: database_name,
            operation,
        })
    }

    pub fn parse_alter_schema(&mut self) -> Result<Statement, ParserError> {
        let schema_name = self.parse_object_name()?;
        let operation = if self.parse_keywords(&[Keyword::OWNER, Keyword::TO]) {
            let owner_name: Ident = self.parse_identifier()?;
            AlterSchemaOperation::ChangeOwner {
                new_owner_name: owner_name,
            }
        } else if self.parse_keyword(Keyword::RENAME) {
            if self.parse_keyword(Keyword::TO) {
                let schema_name = self.parse_object_name()?;
                AlterSchemaOperation::RenameSchema { schema_name }
            } else {
                return self.expected("TO after RENAME", self.peek_token());
            }
        } else {
            return self.expected("RENAME OR OWNER TO after ALTER SCHEMA", self.peek_token());
        };

        Ok(Statement::AlterSchema {
            name: schema_name,
            operation,
        })
    }

    pub fn parse_alter_user(&mut self) -> Result<Statement, ParserError> {
        Ok(Statement::AlterUser(AlterUserStatement::parse_to(self)?))
    }

    pub fn parse_alter_table(&mut self) -> Result<Statement, ParserError> {
        let _ = self.parse_keyword(Keyword::ONLY);
        let table_name = self.parse_object_name()?;
        let operation = if self.parse_keyword(Keyword::ADD) {
            if let Some(constraint) = self.parse_optional_table_constraint()? {
                AlterTableOperation::AddConstraint(constraint)
            } else {
                let _ = self.parse_keyword(Keyword::COLUMN);
                let _if_not_exists =
                    self.parse_keywords(&[Keyword::IF, Keyword::NOT, Keyword::EXISTS]);
                let column_def = self.parse_column_def()?;
                AlterTableOperation::AddColumn { column_def }
            }
        } else if self.parse_keyword(Keyword::RENAME) {
            if self.parse_keyword(Keyword::CONSTRAINT) {
                let old_name = self.parse_identifier_non_reserved()?;
                self.expect_keyword(Keyword::TO)?;
                let new_name = self.parse_identifier_non_reserved()?;
                AlterTableOperation::RenameConstraint { old_name, new_name }
            } else if self.parse_keyword(Keyword::TO) {
                let table_name = self.parse_object_name()?;
                AlterTableOperation::RenameTable { table_name }
            } else {
                let _ = self.parse_keyword(Keyword::COLUMN);
                let old_column_name = self.parse_identifier_non_reserved()?;
                self.expect_keyword(Keyword::TO)?;
                let new_column_name = self.parse_identifier_non_reserved()?;
                AlterTableOperation::RenameColumn {
                    old_column_name,
                    new_column_name,
                }
            }
        } else if self.parse_keywords(&[Keyword::OWNER, Keyword::TO]) {
            let owner_name: Ident = self.parse_identifier()?;
            AlterTableOperation::ChangeOwner {
                new_owner_name: owner_name,
            }
        } else if self.parse_keyword(Keyword::SET) {
            if self.parse_keyword(Keyword::SCHEMA) {
                let schema_name = self.parse_object_name()?;
                AlterTableOperation::SetSchema {
                    new_schema_name: schema_name,
                }
            } else if self.parse_keyword(Keyword::PARALLELISM) {
                if self.expect_keyword(Keyword::TO).is_err()
                    && self.expect_token(&Token::Eq).is_err()
                {
                    return self.expected(
                        "TO or = after ALTER TABLE SET PARALLELISM",
                        self.peek_token(),
                    );
                }

                let value = self.parse_set_variable()?;

                let deferred = self.parse_keyword(Keyword::DEFERRED);

                AlterTableOperation::SetParallelism {
                    parallelism: value,
                    deferred,
                }
            } else if let Some(rate_limit) = self.parse_alter_streaming_rate_limit()? {
                AlterTableOperation::SetStreamingRateLimit { rate_limit }
            } else {
                return self.expected(
                    "SCHEMA/PARALLELISM/STREAMING_RATE_LIMIT after SET",
                    self.peek_token(),
                );
            }
        } else if self.parse_keyword(Keyword::DROP) {
            let _ = self.parse_keyword(Keyword::COLUMN);
            let if_exists = self.parse_keywords(&[Keyword::IF, Keyword::EXISTS]);
            let column_name = self.parse_identifier_non_reserved()?;
            let cascade = self.parse_keyword(Keyword::CASCADE);
            AlterTableOperation::DropColumn {
                column_name,
                if_exists,
                cascade,
            }
        } else if self.parse_keyword(Keyword::ALTER) {
            let _ = self.parse_keyword(Keyword::COLUMN);
            let column_name = self.parse_identifier_non_reserved()?;

            let op = if self.parse_keywords(&[Keyword::SET, Keyword::NOT, Keyword::NULL]) {
                AlterColumnOperation::SetNotNull {}
            } else if self.parse_keywords(&[Keyword::DROP, Keyword::NOT, Keyword::NULL]) {
                AlterColumnOperation::DropNotNull {}
            } else if self.parse_keywords(&[Keyword::SET, Keyword::DEFAULT]) {
                AlterColumnOperation::SetDefault {
                    value: self.parse_expr()?,
                }
            } else if self.parse_keywords(&[Keyword::DROP, Keyword::DEFAULT]) {
                AlterColumnOperation::DropDefault {}
            } else if self.parse_keywords(&[Keyword::SET, Keyword::DATA, Keyword::TYPE])
                || (self.parse_keyword(Keyword::TYPE))
            {
                let data_type = self.parse_data_type()?;
                let using = if self.parse_keyword(Keyword::USING) {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                AlterColumnOperation::SetDataType { data_type, using }
            } else {
                return self.expected(
                    "SET/DROP NOT NULL, SET DEFAULT, SET DATA TYPE after ALTER COLUMN",
                    self.peek_token(),
                );
            };
            AlterTableOperation::AlterColumn { column_name, op }
        } else if self.parse_keywords(&[Keyword::REFRESH, Keyword::SCHEMA]) {
            AlterTableOperation::RefreshSchema
        } else {
            return self.expected(
                "ADD or RENAME or OWNER TO or SET or DROP after ALTER TABLE",
                self.peek_token(),
            );
        };
        Ok(Statement::AlterTable {
            name: table_name,
            operation,
        })
    }

    /// STREAMING_RATE_LIMIT = default | NUMBER
    /// STREAMING_RATE_LIMIT TO default | NUMBER
    pub fn parse_alter_streaming_rate_limit(&mut self) -> Result<Option<i32>, ParserError> {
        if !self.parse_keyword(Keyword::STREAMING_RATE_LIMIT) {
            return Ok(None);
        }
        if self.expect_keyword(Keyword::TO).is_err() && self.expect_token(&Token::Eq).is_err() {
            return self.expected(
                "TO or = after ALTER TABLE SET STREAMING_RATE_LIMIT",
                self.peek_token(),
            );
        }
        let rate_limit = if self.parse_keyword(Keyword::DEFAULT) {
            -1
        } else {
            let s = self.parse_number_value()?;
            if let Ok(n) = s.parse::<i32>() {
                n
            } else {
                return self.expected("number or DEFAULT", self.peek_token());
            }
        };
        Ok(Some(rate_limit))
    }

    pub fn parse_alter_index(&mut self) -> Result<Statement, ParserError> {
        let index_name = self.parse_object_name()?;
        let operation = if self.parse_keyword(Keyword::RENAME) {
            if self.parse_keyword(Keyword::TO) {
                let index_name = self.parse_object_name()?;
                AlterIndexOperation::RenameIndex { index_name }
            } else {
                return self.expected("TO after RENAME", self.peek_token());
            }
        } else if self.parse_keyword(Keyword::SET) {
            if self.parse_keyword(Keyword::PARALLELISM) {
                if self.expect_keyword(Keyword::TO).is_err()
                    && self.expect_token(&Token::Eq).is_err()
                {
                    return self.expected(
                        "TO or = after ALTER TABLE SET PARALLELISM",
                        self.peek_token(),
                    );
                }

                let value = self.parse_set_variable()?;

                let deferred = self.parse_keyword(Keyword::DEFERRED);

                AlterIndexOperation::SetParallelism {
                    parallelism: value,
                    deferred,
                }
            } else {
                return self.expected("PARALLELISM after SET", self.peek_token());
            }
        } else {
            return self.expected("RENAME after ALTER INDEX", self.peek_token());
        };

        Ok(Statement::AlterIndex {
            name: index_name,
            operation,
        })
    }

    pub fn parse_alter_view(&mut self, materialized: bool) -> Result<Statement, ParserError> {
        let view_name = self.parse_object_name()?;
        let operation = if self.parse_keyword(Keyword::RENAME) {
            if self.parse_keyword(Keyword::TO) {
                let view_name = self.parse_object_name()?;
                AlterViewOperation::RenameView { view_name }
            } else {
                return self.expected("TO after RENAME", self.peek_token());
            }
        } else if self.parse_keywords(&[Keyword::OWNER, Keyword::TO]) {
            let owner_name: Ident = self.parse_identifier()?;
            AlterViewOperation::ChangeOwner {
                new_owner_name: owner_name,
            }
        } else if self.parse_keyword(Keyword::SET) {
            if self.parse_keyword(Keyword::SCHEMA) {
                let schema_name = self.parse_object_name()?;
                AlterViewOperation::SetSchema {
                    new_schema_name: schema_name,
                }
            } else if self.parse_keyword(Keyword::PARALLELISM) && materialized {
                if self.expect_keyword(Keyword::TO).is_err()
                    && self.expect_token(&Token::Eq).is_err()
                {
                    return self.expected(
                        "TO or = after ALTER TABLE SET PARALLELISM",
                        self.peek_token(),
                    );
                }

                let value = self.parse_set_variable()?;

                let deferred = self.parse_keyword(Keyword::DEFERRED);

                AlterViewOperation::SetParallelism {
                    parallelism: value,
                    deferred,
                }
            } else if materialized
                && let Some(rate_limit) = self.parse_alter_streaming_rate_limit()?
            {
                AlterViewOperation::SetStreamingRateLimit { rate_limit }
            } else {
                return self.expected(
                    "SCHEMA/PARALLELISM/STREAMING_RATE_LIMIT after SET",
                    self.peek_token(),
                );
            }
        } else {
            return self.expected(
                &format!(
                    "RENAME or OWNER TO or SET after ALTER {}VIEW",
                    if materialized { "MATERIALIZED " } else { "" }
                ),
                self.peek_token(),
            );
        };

        Ok(Statement::AlterView {
            materialized,
            name: view_name,
            operation,
        })
    }

    pub fn parse_alter_sink(&mut self) -> Result<Statement, ParserError> {
        let sink_name = self.parse_object_name()?;
        let operation = if self.parse_keyword(Keyword::RENAME) {
            if self.parse_keyword(Keyword::TO) {
                let sink_name = self.parse_object_name()?;
                AlterSinkOperation::RenameSink { sink_name }
            } else {
                return self.expected("TO after RENAME", self.peek_token());
            }
        } else if self.parse_keywords(&[Keyword::OWNER, Keyword::TO]) {
            let owner_name: Ident = self.parse_identifier()?;
            AlterSinkOperation::ChangeOwner {
                new_owner_name: owner_name,
            }
        } else if self.parse_keyword(Keyword::SET) {
            if self.parse_keyword(Keyword::SCHEMA) {
                let schema_name = self.parse_object_name()?;
                AlterSinkOperation::SetSchema {
                    new_schema_name: schema_name,
                }
            } else if self.parse_keyword(Keyword::PARALLELISM) {
                if self.expect_keyword(Keyword::TO).is_err()
                    && self.expect_token(&Token::Eq).is_err()
                {
                    return self.expected(
                        "TO or = after ALTER TABLE SET PARALLELISM",
                        self.peek_token(),
                    );
                }

                let value = self.parse_set_variable()?;
                let deferred = self.parse_keyword(Keyword::DEFERRED);

                AlterSinkOperation::SetParallelism {
                    parallelism: value,
                    deferred,
                }
            } else {
                return self.expected("SCHEMA/PARALLELISM after SET", self.peek_token());
            }
        } else {
            return self.expected(
                "RENAME or OWNER TO or SET after ALTER SINK",
                self.peek_token(),
            );
        };

        Ok(Statement::AlterSink {
            name: sink_name,
            operation,
        })
    }

    pub fn parse_alter_subscription(&mut self) -> Result<Statement, ParserError> {
        let subscription_name = self.parse_object_name()?;
        let operation = if self.parse_keyword(Keyword::RENAME) {
            if self.parse_keyword(Keyword::TO) {
                let subscription_name = self.parse_object_name()?;
                AlterSubscriptionOperation::RenameSubscription { subscription_name }
            } else {
                return self.expected("TO after RENAME", self.peek_token());
            }
        } else if self.parse_keywords(&[Keyword::OWNER, Keyword::TO]) {
            let owner_name: Ident = self.parse_identifier()?;
            AlterSubscriptionOperation::ChangeOwner {
                new_owner_name: owner_name,
            }
        } else if self.parse_keyword(Keyword::SET) {
            if self.parse_keyword(Keyword::SCHEMA) {
                let schema_name = self.parse_object_name()?;
                AlterSubscriptionOperation::SetSchema {
                    new_schema_name: schema_name,
                }
            } else {
                return self.expected("SCHEMA after SET", self.peek_token());
            }
        } else {
            return self.expected(
                "RENAME or OWNER TO or SET after ALTER SUBSCRIPTION",
                self.peek_token(),
            );
        };

        Ok(Statement::AlterSubscription {
            name: subscription_name,
            operation,
        })
    }

    pub fn parse_alter_source(&mut self) -> Result<Statement, ParserError> {
        let source_name = self.parse_object_name()?;
        let operation = if self.parse_keyword(Keyword::RENAME) {
            if self.parse_keyword(Keyword::TO) {
                let source_name = self.parse_object_name()?;
                AlterSourceOperation::RenameSource { source_name }
            } else {
                return self.expected("TO after RENAME", self.peek_token());
            }
        } else if self.parse_keyword(Keyword::ADD) {
            let _ = self.parse_keyword(Keyword::COLUMN);
            let _if_not_exists = self.parse_keywords(&[Keyword::IF, Keyword::NOT, Keyword::EXISTS]);
            let column_def = self.parse_column_def()?;
            AlterSourceOperation::AddColumn { column_def }
        } else if self.parse_keywords(&[Keyword::OWNER, Keyword::TO]) {
            let owner_name: Ident = self.parse_identifier()?;
            AlterSourceOperation::ChangeOwner {
                new_owner_name: owner_name,
            }
        } else if self.parse_keyword(Keyword::SET) {
            if self.parse_keyword(Keyword::SCHEMA) {
                let schema_name = self.parse_object_name()?;
                AlterSourceOperation::SetSchema {
                    new_schema_name: schema_name,
                }
            } else if let Some(rate_limit) = self.parse_alter_streaming_rate_limit()? {
                AlterSourceOperation::SetStreamingRateLimit { rate_limit }
            } else {
                return self.expected("SCHEMA after SET", self.peek_token());
            }
        } else if self.peek_nth_any_of_keywords(0, &[Keyword::FORMAT]) {
            let connector_schema = self.parse_schema()?.unwrap();
            if connector_schema.key_encode.is_some() {
                return Err(ParserError::ParserError(
                    "key encode clause is not supported in source schema".to_string(),
                ));
            }
            AlterSourceOperation::FormatEncode { connector_schema }
        } else if self.parse_keywords(&[Keyword::REFRESH, Keyword::SCHEMA]) {
            AlterSourceOperation::RefreshSchema
        } else {
            return self.expected(
                "RENAME, ADD COLUMN, OWNER TO, SET or STREAMING_RATE_LIMIT after ALTER SOURCE",
                self.peek_token(),
            );
        };

        Ok(Statement::AlterSource {
            name: source_name,
            operation,
        })
    }

    pub fn parse_alter_function(&mut self) -> Result<Statement, ParserError> {
        let FunctionDesc { name, args } = self.parse_function_desc()?;

        let operation = if self.parse_keyword(Keyword::SET) {
            if self.parse_keyword(Keyword::SCHEMA) {
                let schema_name = self.parse_object_name()?;
                AlterFunctionOperation::SetSchema {
                    new_schema_name: schema_name,
                }
            } else {
                return self.expected("SCHEMA after SET", self.peek_token());
            }
        } else {
            return self.expected("SET after ALTER FUNCTION", self.peek_token());
        };

        Ok(Statement::AlterFunction {
            name,
            args,
            operation,
        })
    }

    pub fn parse_alter_connection(&mut self) -> Result<Statement, ParserError> {
        let connection_name = self.parse_object_name()?;
        let operation = if self.parse_keyword(Keyword::SET) {
            if self.parse_keyword(Keyword::SCHEMA) {
                let schema_name = self.parse_object_name()?;
                AlterConnectionOperation::SetSchema {
                    new_schema_name: schema_name,
                }
            } else {
                return self.expected("SCHEMA after SET", self.peek_token());
            }
        } else {
            return self.expected("SET after ALTER CONNECTION", self.peek_token());
        };

        Ok(Statement::AlterConnection {
            name: connection_name,
            operation,
        })
    }

    pub fn parse_alter_system(&mut self) -> Result<Statement, ParserError> {
        self.expect_keyword(Keyword::SET)?;
        let param = self.parse_identifier()?;
        if self.expect_keyword(Keyword::TO).is_err() && self.expect_token(&Token::Eq).is_err() {
            return self.expected("TO or = after ALTER SYSTEM SET", self.peek_token());
        }
        let value = self.parse_set_variable()?;
        Ok(Statement::AlterSystem { param, value })
    }

    /// Parse a copy statement
    pub fn parse_copy(&mut self) -> Result<Statement, ParserError> {
        let table_name = self.parse_object_name()?;
        let columns = self.parse_parenthesized_column_list(Optional)?;
        self.expect_keywords(&[Keyword::FROM, Keyword::STDIN])?;
        self.expect_token(&Token::SemiColon)?;
        let values = self.parse_tsv();
        Ok(Statement::Copy {
            table_name,
            columns,
            values,
        })
    }

    /// Parse a tab separated values in
    /// COPY payload
    fn parse_tsv(&mut self) -> Vec<Option<String>> {
        self.parse_tab_value()
    }

    fn parse_tab_value(&mut self) -> Vec<Option<String>> {
        let mut values = vec![];
        let mut content = String::from("");
        while let Some(t) = self.next_token_no_skip() {
            match t.token {
                Token::Whitespace(Whitespace::Tab) => {
                    values.push(Some(content.to_string()));
                    content.clear();
                }
                Token::Whitespace(Whitespace::Newline) => {
                    values.push(Some(content.to_string()));
                    content.clear();
                }
                Token::Backslash => {
                    if self.consume_token(&Token::Period) {
                        return values;
                    }
                    if let Token::Word(w) = self.next_token().token {
                        if w.value == "N" {
                            values.push(None);
                        }
                    }
                }
                _ => {
                    content.push_str(&t.to_string());
                }
            }
        }
        values
    }

    /// Parse a literal value (numbers, strings, date/time, booleans)
    fn parse_value(&mut self) -> Result<Value, ParserError> {
        let token = self.next_token();
        match token.token {
            Token::Word(w) => match w.keyword {
                Keyword::TRUE => Ok(Value::Boolean(true)),
                Keyword::FALSE => Ok(Value::Boolean(false)),
                Keyword::NULL => Ok(Value::Null),
                Keyword::NoKeyword if w.quote_style.is_some() => match w.quote_style {
                    Some('"') => Ok(Value::DoubleQuotedString(w.value)),
                    Some('\'') => Ok(Value::SingleQuotedString(w.value)),
                    _ => self.expected("A value?", Token::Word(w).with_location(token.location))?,
                },
                _ => self.expected(
                    "a concrete value",
                    Token::Word(w).with_location(token.location),
                ),
            },
            Token::Number(ref n) => Ok(Value::Number(n.clone())),
            Token::SingleQuotedString(ref s) => Ok(Value::SingleQuotedString(s.to_string())),
            Token::DollarQuotedString(ref s) => Ok(Value::DollarQuotedString(s.clone())),
            Token::CstyleEscapesString(ref s) => Ok(Value::CstyleEscapedString(s.clone())),
            Token::NationalStringLiteral(ref s) => Ok(Value::NationalStringLiteral(s.to_string())),
            Token::HexStringLiteral(ref s) => Ok(Value::HexStringLiteral(s.to_string())),
            unexpected => self.expected("a value", unexpected.with_location(token.location)),
        }
    }

    fn parse_set_variable(&mut self) -> Result<SetVariableValue, ParserError> {
        let mut values = vec![];
        loop {
            let token = self.peek_token();
            let value = match (self.parse_value(), token.token) {
                (Ok(value), _) => SetVariableValueSingle::Literal(value),
                (Err(_), Token::Word(w)) => {
                    if w.keyword == Keyword::DEFAULT {
                        if !values.is_empty() {
                            self.expected(
                                "parameter list value",
                                Token::Word(w).with_location(token.location),
                            )?
                        }
                        return Ok(SetVariableValue::Default);
                    } else {
                        SetVariableValueSingle::Ident(w.to_ident()?)
                    }
                }
                (Err(_), unexpected) => {
                    self.expected("parameter value", unexpected.with_location(token.location))?
                }
            };
            values.push(value);
            if !self.consume_token(&Token::Comma) {
                break;
            }
        }
        if values.len() == 1 {
            Ok(SetVariableValue::Single(values[0].clone()))
        } else {
            Ok(SetVariableValue::List(values))
        }
    }

    pub fn parse_number_value(&mut self) -> Result<String, ParserError> {
        match self.parse_value()? {
            Value::Number(v) => Ok(v),
            _ => {
                self.prev_token();
                self.expected("literal number", self.peek_token())
            }
        }
    }

    /// Parse an unsigned literal integer/long
    pub fn parse_literal_uint(&mut self) -> Result<u64, ParserError> {
        let token = self.next_token();
        match token.token {
            Token::Number(s) => s.parse::<u64>().map_err(|e| {
                ParserError::ParserError(format!("Could not parse '{}' as u64: {}", s, e))
            }),
            unexpected => self.expected("literal int", unexpected.with_location(token.location)),
        }
    }

    pub fn parse_function_definition(&mut self) -> Result<FunctionDefinition, ParserError> {
        let peek_token = self.peek_token();
        match peek_token.token {
            Token::DollarQuotedString(value) => {
                self.next_token();
                Ok(FunctionDefinition::DoubleDollarDef(value.value))
            }
            _ => Ok(FunctionDefinition::SingleQuotedDef(
                self.parse_literal_string()?,
            )),
        }
    }

    /// Parse a literal string
    pub fn parse_literal_string(&mut self) -> Result<String, ParserError> {
        let token = self.next_token();
        match token.token {
            Token::Word(Word {
                value,
                keyword: Keyword::NoKeyword,
                ..
            }) => Ok(value),
            Token::SingleQuotedString(s) => Ok(s),
            unexpected => self.expected("literal string", unexpected.with_location(token.location)),
        }
    }

    /// Parse a map key string
    pub fn parse_map_key(&mut self) -> Result<Expr, ParserError> {
        let token = self.next_token();
        match token.token {
            Token::Word(Word {
                value,
                keyword: Keyword::NoKeyword,
                ..
            }) => {
                if self.peek_token() == Token::LParen {
                    return self.parse_function(ObjectName(vec![Ident::new_unchecked(value)]));
                }
                Ok(Expr::Value(Value::SingleQuotedString(value)))
            }
            Token::SingleQuotedString(s) => Ok(Expr::Value(Value::SingleQuotedString(s))),
            Token::Number(s) => Ok(Expr::Value(Value::Number(s))),
            unexpected => self.expected(
                "literal string, number or function",
                unexpected.with_location(token.location),
            ),
        }
    }

    /// Parse a SQL datatype (in the context of a CREATE TABLE statement for example) and convert
    /// into an array of that datatype if needed
    pub fn parse_data_type(&mut self) -> Result<DataType, ParserError> {
        self.parse_v2(parser_v2::data_type)
    }

    /// Parse `AS identifier` (or simply `identifier` if it's not a reserved keyword)
    /// Some examples with aliases: `SELECT 1 foo`, `SELECT COUNT(*) AS cnt`,
    /// `SELECT ... FROM t1 foo, t2 bar`, `SELECT ... FROM (...) AS bar`
    pub fn parse_optional_alias(
        &mut self,
        reserved_kwds: &[Keyword],
    ) -> Result<Option<Ident>, ParserError> {
        let after_as = self.parse_keyword(Keyword::AS);
        let token = self.next_token();
        match token.token {
            // Accept any identifier after `AS` (though many dialects have restrictions on
            // keywords that may appear here). If there's no `AS`: don't parse keywords,
            // which may start a construct allowed in this position, to be parsed as aliases.
            // (For example, in `FROM t1 JOIN` the `JOIN` will always be parsed as a keyword,
            // not an alias.)
            Token::Word(w) if after_as || (!reserved_kwds.contains(&w.keyword)) => {
                Ok(Some(w.to_ident()?))
            }
            not_an_ident => {
                if after_as {
                    return self.expected(
                        "an identifier after AS",
                        not_an_ident.with_location(token.location),
                    );
                }
                self.prev_token();
                Ok(None) // no alias found
            }
        }
    }

    /// Parse `AS identifier` when the AS is describing a table-valued object,
    /// like in `... FROM generate_series(1, 10) AS t (col)`. In this case
    /// the alias is allowed to optionally name the columns in the table, in
    /// addition to the table itself.
    pub fn parse_optional_table_alias(
        &mut self,
        reserved_kwds: &[Keyword],
    ) -> Result<Option<TableAlias>, ParserError> {
        match self.parse_optional_alias(reserved_kwds)? {
            Some(name) => {
                let columns = self.parse_parenthesized_column_list(Optional)?;
                Ok(Some(TableAlias { name, columns }))
            }
            None => Ok(None),
        }
    }

    /// syntax `FOR SYSTEM_TIME AS OF PROCTIME()` is used for temporal join.
    pub fn parse_as_of(&mut self) -> Result<Option<AsOf>, ParserError> {
        let after_for = self.parse_keyword(Keyword::FOR);
        if after_for {
            if self.peek_nth_any_of_keywords(0, &[Keyword::SYSTEM_TIME]) {
                self.expect_keywords(&[Keyword::SYSTEM_TIME, Keyword::AS, Keyword::OF])?;
                let token = self.next_token();
                match token.token {
                    Token::Word(w) => {
                        let ident = w.to_ident()?;
                        // Backward compatibility for now.
                        if ident.real_value() == "proctime" || ident.real_value() == "now" {
                            self.expect_token(&Token::LParen)?;
                            self.expect_token(&Token::RParen)?;
                            Ok(Some(AsOf::ProcessTime))
                        } else {
                            parser_err!(format!("Expected proctime, found: {}", ident.real_value()))
                        }
                    }
                    Token::Number(s) => {
                        let num = s.parse::<i64>().map_err(|e| {
                            ParserError::ParserError(format!(
                                "Could not parse '{}' as i64: {}",
                                s, e
                            ))
                        });
                        Ok(Some(AsOf::TimestampNum(num?)))
                    }
                    Token::SingleQuotedString(s) => Ok(Some(AsOf::TimestampString(s))),
                    unexpected => self.expected(
                        "Proctime(), Number or SingleQuotedString",
                        unexpected.with_location(token.location),
                    ),
                }
            } else {
                self.expect_keywords(&[Keyword::SYSTEM_VERSION, Keyword::AS, Keyword::OF])?;
                let token = self.next_token();
                match token.token {
                    Token::Number(s) => {
                        let num = s.parse::<i64>().map_err(|e| {
                            ParserError::ParserError(format!(
                                "Could not parse '{}' as i64: {}",
                                s, e
                            ))
                        });
                        Ok(Some(AsOf::VersionNum(num?)))
                    }
                    Token::SingleQuotedString(s) => Ok(Some(AsOf::VersionString(s))),
                    unexpected => self.expected(
                        "Number or SingleQuotedString",
                        unexpected.with_location(token.location),
                    ),
                }
            }
        } else {
            Ok(None)
        }
    }

    /// Parse a possibly qualified, possibly quoted identifier, e.g.
    /// `foo` or `myschema."table"
    pub fn parse_object_name(&mut self) -> Result<ObjectName, ParserError> {
        let mut idents = vec![];
        loop {
            idents.push(self.parse_identifier()?);
            if !self.consume_token(&Token::Period) {
                break;
            }
        }
        Ok(ObjectName(idents))
    }

    /// Parse identifiers strictly i.e. don't parse keywords
    pub fn parse_identifiers_non_keywords(&mut self) -> Result<Vec<Ident>, ParserError> {
        let mut idents = vec![];
        loop {
            match self.peek_token().token {
                Token::Word(w) => {
                    if w.keyword != Keyword::NoKeyword {
                        break;
                    }

                    idents.push(w.to_ident()?);
                }
                Token::EOF | Token::Eq => break,
                _ => {}
            }

            self.next_token();
        }

        Ok(idents)
    }

    /// Parse identifiers
    pub fn parse_identifiers(&mut self) -> Result<Vec<Ident>, ParserError> {
        let mut idents = vec![];
        loop {
            let token = self.next_token();
            match token.token {
                Token::Word(w) => {
                    idents.push(w.to_ident()?);
                }
                Token::EOF => break,
                _ => {}
            }
        }

        Ok(idents)
    }

    /// Parse a simple one-word identifier (possibly quoted, possibly a keyword)
    pub fn parse_identifier(&mut self) -> Result<Ident, ParserError> {
        let token = self.next_token();
        match token.token {
            Token::Word(w) => Ok(w.to_ident()?),
            unexpected => self.expected("identifier", unexpected.with_location(token.location)),
        }
    }

    /// Parse a simple one-word identifier (possibly quoted, possibly a non-reserved keyword)
    pub fn parse_identifier_non_reserved(&mut self) -> Result<Ident, ParserError> {
        let token = self.next_token();
        match token.token.clone() {
            Token::Word(w) => {
                match keywords::RESERVED_FOR_COLUMN_OR_TABLE_NAME.contains(&w.keyword) {
                    true => parser_err!(format!("syntax error at or near {token}")),
                    false => Ok(w.to_ident()?),
                }
            }
            unexpected => self.expected("identifier", unexpected.with_location(token.location)),
        }
    }

    /// Parse a parenthesized comma-separated list of unqualified, possibly quoted identifiers
    pub fn parse_parenthesized_column_list(
        &mut self,
        optional: IsOptional,
    ) -> Result<Vec<Ident>, ParserError> {
        if self.consume_token(&Token::LParen) {
            let cols = self.parse_comma_separated(Parser::parse_identifier_non_reserved)?;
            self.expect_token(&Token::RParen)?;
            Ok(cols)
        } else if optional == Optional {
            Ok(vec![])
        } else {
            self.expected("a list of columns in parentheses", self.peek_token())
        }
    }

    pub fn parse_returning(
        &mut self,
        optional: IsOptional,
    ) -> Result<Vec<SelectItem>, ParserError> {
        if self.parse_keyword(Keyword::RETURNING) {
            let cols = self.parse_comma_separated(Parser::parse_select_item)?;
            Ok(cols)
        } else if optional == Optional {
            Ok(vec![])
        } else {
            self.expected("a list of columns or * after returning", self.peek_token())
        }
    }

    pub fn parse_row_expr(&mut self) -> Result<Expr, ParserError> {
        Ok(Expr::Row(self.parse_token_wrapped_exprs(
            &Token::LParen,
            &Token::RParen,
        )?))
    }

    /// Parse a comma-separated list (maybe empty) from a wrapped expression
    pub fn parse_token_wrapped_exprs(
        &mut self,
        left: &Token,
        right: &Token,
    ) -> Result<Vec<Expr>, ParserError> {
        if self.consume_token(left) {
            let exprs = if self.consume_token(right) {
                vec![]
            } else {
                let exprs = self.parse_comma_separated(Parser::parse_expr)?;
                self.expect_token(right)?;
                exprs
            };
            Ok(exprs)
        } else {
            self.expected(left.to_string().as_str(), self.peek_token())
        }
    }

    pub fn parse_optional_precision(&mut self) -> Result<Option<u64>, ParserError> {
        if self.consume_token(&Token::LParen) {
            let n = self.parse_literal_uint()?;
            self.expect_token(&Token::RParen)?;
            Ok(Some(n))
        } else {
            Ok(None)
        }
    }

    pub fn parse_optional_precision_scale(
        &mut self,
    ) -> Result<(Option<u64>, Option<u64>), ParserError> {
        if self.consume_token(&Token::LParen) {
            let n = self.parse_literal_uint()?;
            let scale = if self.consume_token(&Token::Comma) {
                Some(self.parse_literal_uint()?)
            } else {
                None
            };
            self.expect_token(&Token::RParen)?;
            Ok((Some(n), scale))
        } else {
            Ok((None, None))
        }
    }

    pub fn parse_delete(&mut self) -> Result<Statement, ParserError> {
        self.expect_keyword(Keyword::FROM)?;
        let table_name = self.parse_object_name()?;
        let selection = if self.parse_keyword(Keyword::WHERE) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let returning = self.parse_returning(Optional)?;

        Ok(Statement::Delete {
            table_name,
            selection,
            returning,
        })
    }

    pub fn parse_optional_boolean(&mut self, default: bool) -> bool {
        if let Some(keyword) = self.parse_one_of_keywords(&[Keyword::TRUE, Keyword::FALSE]) {
            match keyword {
                Keyword::TRUE => true,
                Keyword::FALSE => false,
                _ => unreachable!(),
            }
        } else {
            default
        }
    }

    pub fn parse_explain(&mut self) -> Result<Statement, ParserError> {
        let mut options = ExplainOptions::default();

        let explain_key_words = [
            Keyword::VERBOSE,
            Keyword::TRACE,
            Keyword::TYPE,
            Keyword::LOGICAL,
            Keyword::PHYSICAL,
            Keyword::DISTSQL,
        ];

        let parse_explain_option = |parser: &mut Parser| -> Result<(), ParserError> {
            let keyword = parser.expect_one_of_keywords(&explain_key_words)?;
            match keyword {
                Keyword::VERBOSE => options.verbose = parser.parse_optional_boolean(true),
                Keyword::TRACE => options.trace = parser.parse_optional_boolean(true),
                Keyword::TYPE => {
                    let explain_type = parser.expect_one_of_keywords(&[
                        Keyword::LOGICAL,
                        Keyword::PHYSICAL,
                        Keyword::DISTSQL,
                    ])?;
                    match explain_type {
                        Keyword::LOGICAL => options.explain_type = ExplainType::Logical,
                        Keyword::PHYSICAL => options.explain_type = ExplainType::Physical,
                        Keyword::DISTSQL => options.explain_type = ExplainType::DistSql,
                        _ => unreachable!("{}", keyword),
                    }
                }
                Keyword::LOGICAL => options.explain_type = ExplainType::Logical,
                Keyword::PHYSICAL => options.explain_type = ExplainType::Physical,
                Keyword::DISTSQL => options.explain_type = ExplainType::DistSql,
                _ => unreachable!("{}", keyword),
            };
            Ok(())
        };

        let analyze = self.parse_keyword(Keyword::ANALYZE);
        // In order to support following statement, we need to peek before consume.
        // explain (select 1) union (select 1)
        if self.peek_token() == Token::LParen
            && self.peek_nth_any_of_keywords(1, &explain_key_words)
            && self.consume_token(&Token::LParen)
        {
            self.parse_comma_separated(parse_explain_option)?;
            self.expect_token(&Token::RParen)?;
        }

        let statement = self.parse_statement()?;
        Ok(Statement::Explain {
            analyze,
            statement: Box::new(statement),
            options,
        })
    }

    /// Parse a query expression, i.e. a `SELECT` statement optionally
    /// preceded with some `WITH` CTE declarations and optionally followed
    /// by `ORDER BY`. Unlike some other parse_... methods, this one doesn't
    /// expect the initial keyword to be already consumed
    pub fn parse_query(&mut self) -> Result<Query, ParserError> {
        let with = if self.parse_keyword(Keyword::WITH) {
            Some(With {
                recursive: self.parse_keyword(Keyword::RECURSIVE),
                cte_tables: self.parse_comma_separated(Parser::parse_cte)?,
            })
        } else {
            None
        };

        let body = self.parse_query_body(0)?;

        let order_by = if self.parse_keywords(&[Keyword::ORDER, Keyword::BY]) {
            self.parse_comma_separated(Parser::parse_order_by_expr)?
        } else {
            vec![]
        };

        let limit = if self.parse_keyword(Keyword::LIMIT) {
            self.parse_limit()?
        } else {
            None
        };

        let offset = if self.parse_keyword(Keyword::OFFSET) {
            Some(self.parse_offset()?)
        } else {
            None
        };

        let fetch = if self.parse_keyword(Keyword::FETCH) {
            if limit.is_some() {
                return parser_err!("Cannot specify both LIMIT and FETCH".to_string());
            }
            let fetch = self.parse_fetch()?;
            if fetch.with_ties && order_by.is_empty() {
                return parser_err!(
                    "WITH TIES cannot be specified without ORDER BY clause".to_string()
                );
            }
            Some(fetch)
        } else {
            None
        };

        Ok(Query {
            with,
            body,
            order_by,
            limit,
            offset,
            fetch,
        })
    }

    /// Parse a CTE (`alias [( col1, col2, ... )] AS (subquery)`)
    fn parse_cte(&mut self) -> Result<Cte, ParserError> {
        let name = self.parse_identifier_non_reserved()?;

        let mut cte = if self.parse_keyword(Keyword::AS) {
            self.expect_token(&Token::LParen)?;
            let query = self.parse_query()?;
            self.expect_token(&Token::RParen)?;
            let alias = TableAlias {
                name,
                columns: vec![],
            };
            Cte {
                alias,
                query,
                from: None,
            }
        } else {
            let columns = self.parse_parenthesized_column_list(Optional)?;
            self.expect_keyword(Keyword::AS)?;
            self.expect_token(&Token::LParen)?;
            let query = self.parse_query()?;
            self.expect_token(&Token::RParen)?;
            let alias = TableAlias { name, columns };
            Cte {
                alias,
                query,
                from: None,
            }
        };
        if self.parse_keyword(Keyword::FROM) {
            cte.from = Some(self.parse_identifier()?);
        }
        Ok(cte)
    }

    /// Parse a "query body", which is an expression with roughly the
    /// following grammar:
    /// ```text
    ///   query_body ::= restricted_select | '(' subquery ')' | set_operation
    ///   restricted_select ::= 'SELECT' [expr_list] [ from ] [ where ] [ groupby_having ]
    ///   subquery ::= query_body [ order_by_limit ]
    ///   set_operation ::= query_body { 'UNION' | 'EXCEPT' | 'INTERSECT' } [ 'ALL' ] query_body
    /// ```
    fn parse_query_body(&mut self, precedence: u8) -> Result<SetExpr, ParserError> {
        // We parse the expression using a Pratt parser, as in `parse_expr()`.
        // Start by parsing a restricted SELECT or a `(subquery)`:
        let mut expr = if self.parse_keyword(Keyword::SELECT) {
            SetExpr::Select(Box::new(self.parse_select()?))
        } else if self.consume_token(&Token::LParen) {
            // CTEs are not allowed here, but the parser currently accepts them
            let subquery = self.parse_query()?;
            self.expect_token(&Token::RParen)?;
            SetExpr::Query(Box::new(subquery))
        } else if self.parse_keyword(Keyword::VALUES) {
            SetExpr::Values(self.parse_values()?)
        } else {
            return self.expected(
                "SELECT, VALUES, or a subquery in the query body",
                self.peek_token(),
            );
        };

        loop {
            // The query can be optionally followed by a set operator:
            let op = self.parse_set_operator(&self.peek_token().token);
            let next_precedence = match op {
                // UNION and EXCEPT have the same binding power and evaluate left-to-right
                Some(SetOperator::Union) | Some(SetOperator::Except) => 10,
                // INTERSECT has higher precedence than UNION/EXCEPT
                Some(SetOperator::Intersect) => 20,
                // Unexpected token or EOF => stop parsing the query body
                None => break,
            };
            if precedence >= next_precedence {
                break;
            }
            self.next_token(); // skip past the set operator
            expr = SetExpr::SetOperation {
                left: Box::new(expr),
                op: op.unwrap(),
                all: self.parse_keyword(Keyword::ALL),
                right: Box::new(self.parse_query_body(next_precedence)?),
            };
        }

        Ok(expr)
    }

    fn parse_set_operator(&mut self, token: &Token) -> Option<SetOperator> {
        match token {
            Token::Word(w) if w.keyword == Keyword::UNION => Some(SetOperator::Union),
            Token::Word(w) if w.keyword == Keyword::EXCEPT => Some(SetOperator::Except),
            Token::Word(w) if w.keyword == Keyword::INTERSECT => Some(SetOperator::Intersect),
            _ => None,
        }
    }

    /// Parse a restricted `SELECT` statement (no CTEs / `UNION` / `ORDER BY`),
    /// assuming the initial `SELECT` was already consumed
    pub fn parse_select(&mut self) -> Result<Select, ParserError> {
        let distinct = self.parse_all_or_distinct_on()?;

        let projection = self.parse_comma_separated(Parser::parse_select_item)?;

        // Note that for keywords to be properly handled here, they need to be
        // added to `RESERVED_FOR_COLUMN_ALIAS` / `RESERVED_FOR_TABLE_ALIAS`,
        // otherwise they may be parsed as an alias as part of the `projection`
        // or `from`.

        let from = if self.parse_keyword(Keyword::FROM) {
            self.parse_comma_separated(Parser::parse_table_and_joins)?
        } else {
            vec![]
        };
        let mut lateral_views = vec![];
        loop {
            if self.parse_keywords(&[Keyword::LATERAL, Keyword::VIEW]) {
                let outer = self.parse_keyword(Keyword::OUTER);
                let lateral_view = self.parse_expr()?;
                let lateral_view_name = self.parse_object_name()?;
                let lateral_col_alias = self
                    .parse_comma_separated(|parser| {
                        parser.parse_optional_alias(&[
                            Keyword::WHERE,
                            Keyword::GROUP,
                            Keyword::CLUSTER,
                            Keyword::HAVING,
                            Keyword::LATERAL,
                        ]) // This couldn't possibly be a bad idea
                    })?
                    .into_iter()
                    .flatten()
                    .collect();

                lateral_views.push(LateralView {
                    lateral_view,
                    lateral_view_name,
                    lateral_col_alias,
                    outer,
                });
            } else {
                break;
            }
        }

        let selection = if self.parse_keyword(Keyword::WHERE) {
            Some(self.parse_expr()?)
        } else {
            None
        };

        let group_by = if self.parse_keywords(&[Keyword::GROUP, Keyword::BY]) {
            self.parse_comma_separated(Parser::parse_group_by_expr)?
        } else {
            vec![]
        };

        let having = if self.parse_keyword(Keyword::HAVING) {
            Some(self.parse_expr()?)
        } else {
            None
        };

        Ok(Select {
            distinct,
            projection,
            from,
            lateral_views,
            selection,
            group_by,
            having,
        })
    }

    pub fn parse_set(&mut self) -> Result<Statement, ParserError> {
        let modifier = self.parse_one_of_keywords(&[Keyword::SESSION, Keyword::LOCAL]);
        if self.parse_keywords(&[Keyword::TIME, Keyword::ZONE]) {
            let token = self.peek_token();
            let value = match (self.parse_value(), token.token) {
                (Ok(value), _) => SetTimeZoneValue::Literal(value),
                (Err(_), Token::Word(w)) if w.keyword == Keyword::DEFAULT => {
                    SetTimeZoneValue::Default
                }
                (Err(_), Token::Word(w)) if w.keyword == Keyword::LOCAL => SetTimeZoneValue::Local,
                (Err(_), Token::Word(w)) => SetTimeZoneValue::Ident(w.to_ident()?),
                (Err(_), unexpected) => {
                    self.expected("variable value", unexpected.with_location(token.location))?
                }
            };

            Ok(Statement::SetTimeZone {
                local: modifier == Some(Keyword::LOCAL),
                value,
            })
        } else if self.parse_keyword(Keyword::CHARACTERISTICS) && modifier == Some(Keyword::SESSION)
        {
            self.expect_keywords(&[Keyword::AS, Keyword::TRANSACTION])?;
            Ok(Statement::SetTransaction {
                modes: self.parse_transaction_modes()?,
                snapshot: None,
                session: true,
            })
        } else if self.parse_keyword(Keyword::TRANSACTION) && modifier.is_none() {
            if self.parse_keyword(Keyword::SNAPSHOT) {
                let snapshot_id = self.parse_value()?;
                return Ok(Statement::SetTransaction {
                    modes: vec![],
                    snapshot: Some(snapshot_id),
                    session: false,
                });
            }
            Ok(Statement::SetTransaction {
                modes: self.parse_transaction_modes()?,
                snapshot: None,
                session: false,
            })
        } else {
            let variable = self.parse_identifier()?;

            if self.consume_token(&Token::Eq) || self.parse_keyword(Keyword::TO) {
                let value = self.parse_set_variable()?;
                Ok(Statement::SetVariable {
                    local: modifier == Some(Keyword::LOCAL),
                    variable,
                    value,
                })
            } else {
                self.expected("equals sign or TO", self.peek_token())
            }
        }
    }

    /// If have `databases`,`tables`,`columns`,`schemas` and `materialized views` after show,
    /// return `Statement::ShowCommand` or `Statement::ShowColumn`,
    /// otherwise, return `Statement::ShowVariable`.
    pub fn parse_show(&mut self) -> Result<Statement, ParserError> {
        let index = self.index;
        if let Token::Word(w) = self.next_token().token {
            match w.keyword {
                Keyword::TABLES => {
                    return Ok(Statement::ShowObjects {
                        object: ShowObject::Table {
                            schema: self.parse_from_and_identifier()?,
                        },
                        filter: self.parse_show_statement_filter()?,
                    });
                }
                Keyword::INTERNAL => {
                    self.expect_keyword(Keyword::TABLES)?;
                    return Ok(Statement::ShowObjects {
                        object: ShowObject::InternalTable {
                            schema: self.parse_from_and_identifier()?,
                        },
                        filter: self.parse_show_statement_filter()?,
                    });
                }
                Keyword::SOURCES => {
                    return Ok(Statement::ShowObjects {
                        object: ShowObject::Source {
                            schema: self.parse_from_and_identifier()?,
                        },
                        filter: self.parse_show_statement_filter()?,
                    });
                }
                Keyword::SINKS => {
                    return Ok(Statement::ShowObjects {
                        object: ShowObject::Sink {
                            schema: self.parse_from_and_identifier()?,
                        },
                        filter: self.parse_show_statement_filter()?,
                    });
                }
                Keyword::SUBSCRIPTIONS => {
                    return Ok(Statement::ShowObjects {
                        object: ShowObject::Subscription {
                            schema: self.parse_from_and_identifier()?,
                        },
                        filter: self.parse_show_statement_filter()?,
                    });
                }
                Keyword::DATABASES => {
                    return Ok(Statement::ShowObjects {
                        object: ShowObject::Database,
                        filter: self.parse_show_statement_filter()?,
                    });
                }
                Keyword::SCHEMAS => {
                    return Ok(Statement::ShowObjects {
                        object: ShowObject::Schema,
                        filter: self.parse_show_statement_filter()?,
                    });
                }
                Keyword::VIEWS => {
                    return Ok(Statement::ShowObjects {
                        object: ShowObject::View {
                            schema: self.parse_from_and_identifier()?,
                        },
                        filter: self.parse_show_statement_filter()?,
                    });
                }
                Keyword::MATERIALIZED => {
                    if self.parse_keyword(Keyword::VIEWS) {
                        return Ok(Statement::ShowObjects {
                            object: ShowObject::MaterializedView {
                                schema: self.parse_from_and_identifier()?,
                            },
                            filter: self.parse_show_statement_filter()?,
                        });
                    } else {
                        return self.expected("VIEWS after MATERIALIZED", self.peek_token());
                    }
                }
                Keyword::COLUMNS => {
                    if self.parse_keyword(Keyword::FROM) {
                        return Ok(Statement::ShowObjects {
                            object: ShowObject::Columns {
                                table: self.parse_object_name()?,
                            },
                            filter: self.parse_show_statement_filter()?,
                        });
                    } else {
                        return self.expected("from after columns", self.peek_token());
                    }
                }
                Keyword::CONNECTIONS => {
                    return Ok(Statement::ShowObjects {
                        object: ShowObject::Connection {
                            schema: self.parse_from_and_identifier()?,
                        },
                        filter: self.parse_show_statement_filter()?,
                    });
                }
                Keyword::FUNCTIONS => {
                    return Ok(Statement::ShowObjects {
                        object: ShowObject::Function {
                            schema: self.parse_from_and_identifier()?,
                        },
                        filter: self.parse_show_statement_filter()?,
                    });
                }
                Keyword::INDEXES => {
                    if self.parse_keyword(Keyword::FROM) {
                        return Ok(Statement::ShowObjects {
                            object: ShowObject::Indexes {
                                table: self.parse_object_name()?,
                            },
                            filter: self.parse_show_statement_filter()?,
                        });
                    } else {
                        return self.expected("from after indexes", self.peek_token());
                    }
                }
                Keyword::CLUSTER => {
                    return Ok(Statement::ShowObjects {
                        object: ShowObject::Cluster,
                        filter: self.parse_show_statement_filter()?,
                    });
                }
                Keyword::JOBS => {
                    return Ok(Statement::ShowObjects {
                        object: ShowObject::Jobs,
                        filter: self.parse_show_statement_filter()?,
                    });
                }
                Keyword::PROCESSLIST => {
                    return Ok(Statement::ShowObjects {
                        object: ShowObject::ProcessList,
                        filter: self.parse_show_statement_filter()?,
                    });
                }
                Keyword::TRANSACTION => {
                    self.expect_keywords(&[Keyword::ISOLATION, Keyword::LEVEL])?;
                    return Ok(Statement::ShowTransactionIsolationLevel);
                }
                _ => {}
            }
        }
        self.index = index;
        Ok(Statement::ShowVariable {
            variable: self.parse_identifiers()?,
        })
    }

    pub fn parse_cancel_job(&mut self) -> Result<Statement, ParserError> {
        // CANCEL [JOBS|JOB] job_ids
        match self.peek_token().token {
            Token::Word(w) if Keyword::JOBS == w.keyword || Keyword::JOB == w.keyword => {
                self.next_token();
            }
            _ => return self.expected("JOBS or JOB after CANCEL", self.peek_token()),
        }

        let mut job_ids = vec![];
        loop {
            job_ids.push(self.parse_literal_uint()? as u32);
            if !self.consume_token(&Token::Comma) {
                break;
            }
        }
        Ok(Statement::CancelJobs(JobIdents(job_ids)))
    }

    pub fn parse_kill_process(&mut self) -> Result<Statement, ParserError> {
        let process_id = self.parse_literal_uint()? as i32;
        Ok(Statement::Kill(process_id))
    }

    /// Parser `from schema` after `show tables` and `show materialized views`, if not conclude
    /// `from` then use default schema name.
    pub fn parse_from_and_identifier(&mut self) -> Result<Option<Ident>, ParserError> {
        if self.parse_keyword(Keyword::FROM) {
            Ok(Some(self.parse_identifier_non_reserved()?))
        } else {
            Ok(None)
        }
    }

    /// Parse object type and name after `show create`.
    pub fn parse_show_create(&mut self) -> Result<Statement, ParserError> {
        if let Token::Word(w) = self.next_token().token {
            let show_type = match w.keyword {
                Keyword::TABLE => ShowCreateType::Table,
                Keyword::MATERIALIZED => {
                    if self.parse_keyword(Keyword::VIEW) {
                        ShowCreateType::MaterializedView
                    } else {
                        return self.expected("VIEW after MATERIALIZED", self.peek_token());
                    }
                }
                Keyword::VIEW => ShowCreateType::View,
                Keyword::INDEX => ShowCreateType::Index,
                Keyword::SOURCE => ShowCreateType::Source,
                Keyword::SINK => ShowCreateType::Sink,
                Keyword::SUBSCRIPTION => ShowCreateType::Subscription,
                Keyword::FUNCTION => ShowCreateType::Function,
                _ => return self.expected(
                    "TABLE, MATERIALIZED VIEW, VIEW, INDEX, FUNCTION, SOURCE, SUBSCRIPTION or SINK",
                    self.peek_token(),
                ),
            };
            return Ok(Statement::ShowCreateObject {
                create_type: show_type,
                name: self.parse_object_name()?,
            });
        }
        self.expected(
            "TABLE, MATERIALIZED VIEW, VIEW, INDEX, FUNCTION, SOURCE, SUBSCRIPTION or SINK",
            self.peek_token(),
        )
    }

    pub fn parse_show_statement_filter(
        &mut self,
    ) -> Result<Option<ShowStatementFilter>, ParserError> {
        if self.parse_keyword(Keyword::LIKE) {
            Ok(Some(ShowStatementFilter::Like(
                self.parse_literal_string()?,
            )))
        } else if self.parse_keyword(Keyword::ILIKE) {
            Ok(Some(ShowStatementFilter::ILike(
                self.parse_literal_string()?,
            )))
        } else if self.parse_keyword(Keyword::WHERE) {
            Ok(Some(ShowStatementFilter::Where(self.parse_expr()?)))
        } else {
            Ok(None)
        }
    }

    pub fn parse_table_and_joins(&mut self) -> Result<TableWithJoins, ParserError> {
        let relation = self.parse_table_factor()?;

        // Note that for keywords to be properly handled here, they need to be
        // added to `RESERVED_FOR_TABLE_ALIAS`, otherwise they may be parsed as
        // a table alias.
        let mut joins = vec![];
        loop {
            let join = if self.parse_keyword(Keyword::CROSS) {
                let join_operator = if self.parse_keyword(Keyword::JOIN) {
                    JoinOperator::CrossJoin
                } else {
                    return self.expected("JOIN after CROSS", self.peek_token());
                };
                Join {
                    relation: self.parse_table_factor()?,
                    join_operator,
                }
            } else {
                let natural = self.parse_keyword(Keyword::NATURAL);
                let peek_keyword = if let Token::Word(w) = self.peek_token().token {
                    w.keyword
                } else {
                    Keyword::NoKeyword
                };

                let join_operator_type = match peek_keyword {
                    Keyword::INNER | Keyword::JOIN => {
                        let _ = self.parse_keyword(Keyword::INNER);
                        self.expect_keyword(Keyword::JOIN)?;
                        JoinOperator::Inner
                    }
                    kw @ Keyword::LEFT | kw @ Keyword::RIGHT | kw @ Keyword::FULL => {
                        let _ = self.next_token();
                        let _ = self.parse_keyword(Keyword::OUTER);
                        self.expect_keyword(Keyword::JOIN)?;
                        match kw {
                            Keyword::LEFT => JoinOperator::LeftOuter,
                            Keyword::RIGHT => JoinOperator::RightOuter,
                            Keyword::FULL => JoinOperator::FullOuter,
                            _ => unreachable!(),
                        }
                    }
                    Keyword::OUTER => {
                        return self.expected("LEFT, RIGHT, or FULL", self.peek_token());
                    }
                    _ if natural => {
                        return self.expected("a join type after NATURAL", self.peek_token());
                    }
                    _ => break,
                };
                let relation = self.parse_table_factor()?;
                let join_constraint = self.parse_join_constraint(natural)?;
                let join_operator = join_operator_type(join_constraint);
                if let JoinOperator::Inner(JoinConstraint::None) = join_operator {
                    return self.expected("join constraint after INNER JOIN", self.peek_token());
                }
                Join {
                    relation,
                    join_operator,
                }
            };
            joins.push(join);
        }
        Ok(TableWithJoins { relation, joins })
    }

    /// A table name or a parenthesized subquery, followed by optional `[AS] alias`
    pub fn parse_table_factor(&mut self) -> Result<TableFactor, ParserError> {
        if self.parse_keyword(Keyword::LATERAL) {
            // LATERAL must always be followed by a subquery.
            if !self.consume_token(&Token::LParen) {
                self.expected("subquery after LATERAL", self.peek_token())?;
            }
            self.parse_derived_table_factor(Lateral)
        } else if self.consume_token(&Token::LParen) {
            // A left paren introduces either a derived table (i.e., a subquery)
            // or a nested join. It's nearly impossible to determine ahead of
            // time which it is... so we just try to parse both.
            //
            // Here's an example that demonstrates the complexity:
            //                     /-------------------------------------------------------\
            //                     | /-----------------------------------\                 |
            //     SELECT * FROM ( ( ( (SELECT 1) UNION (SELECT 2) ) AS t1 NATURAL JOIN t2 ) )
            //                   ^ ^ ^ ^
            //                   | | | |
            //                   | | | |
            //                   | | | (4) belongs to a SetExpr::Query inside the subquery
            //                   | | (3) starts a derived table (subquery)
            //                   | (2) starts a nested join
            //                   (1) an additional set of parens around a nested join
            //

            // It can only be a subquery. We don't use `maybe_parse` so that a meaningful error can
            // be returned.
            match self.peek_token().token {
                Token::Word(w)
                    if [Keyword::SELECT, Keyword::WITH, Keyword::VALUES].contains(&w.keyword) =>
                {
                    return self.parse_derived_table_factor(NotLateral);
                }
                _ => {}
            };
            // It can still be a subquery, e.g., the case (3) in the example above:
            // (SELECT 1) UNION (SELECT 2)
            // TODO: how to produce a good error message here?
            if self.peek_token() == Token::LParen {
                return_ok_if_some!(
                    self.maybe_parse(|parser| parser.parse_derived_table_factor(NotLateral))
                );
            }

            // A parsing error from `parse_derived_table_factor` indicates that the '(' we've
            // recently consumed does not start a derived table (cases 1, 2, or 4).
            // `maybe_parse` will ignore such an error and rewind to be after the opening '('.

            // Inside the parentheses we expect to find an (A) table factor
            // followed by some joins or (B) another level of nesting.
            let table_and_joins = self.parse_table_and_joins()?;

            #[allow(clippy::if_same_then_else)]
            if !table_and_joins.joins.is_empty() {
                self.expect_token(&Token::RParen)?;
                Ok(TableFactor::NestedJoin(Box::new(table_and_joins))) // (A)
            } else if let TableFactor::NestedJoin(_) = &table_and_joins.relation {
                // (B): `table_and_joins` (what we found inside the parentheses)
                // is a nested join `(foo JOIN bar)`, not followed by other joins.
                self.expect_token(&Token::RParen)?;
                Ok(TableFactor::NestedJoin(Box::new(table_and_joins)))
            } else {
                // The SQL spec prohibits derived tables and bare tables from
                // appearing alone in parentheses (e.g. `FROM (mytable)`)
                parser_err!(format!(
                    "Expected joined table, found: {table_and_joins}, next_token: {}",
                    self.peek_token()
                ))
            }
        } else {
            let name = self.parse_object_name()?;
            // Postgres,table-valued functions:
            if self.consume_token(&Token::LParen) {
                // ignore VARIADIC here
                let (args, order_by, _variadic) = self.parse_optional_args()?;
                // Table-valued functions do not support ORDER BY, should return error if it appears
                if !order_by.is_empty() {
                    return parser_err!("Table-valued functions do not support ORDER BY clauses");
                }
                let with_ordinality = self.parse_keywords(&[Keyword::WITH, Keyword::ORDINALITY]);

                let alias = self.parse_optional_table_alias(keywords::RESERVED_FOR_TABLE_ALIAS)?;
                Ok(TableFactor::TableFunction {
                    name,
                    alias,
                    args,
                    with_ordinality,
                })
            } else {
                let as_of = self.parse_as_of()?;
                let alias = self.parse_optional_table_alias(keywords::RESERVED_FOR_TABLE_ALIAS)?;
                Ok(TableFactor::Table { name, alias, as_of })
            }
        }
    }

    pub fn parse_derived_table_factor(
        &mut self,
        lateral: IsLateral,
    ) -> Result<TableFactor, ParserError> {
        let subquery = Box::new(self.parse_query()?);
        self.expect_token(&Token::RParen)?;
        let alias = self.parse_optional_table_alias(keywords::RESERVED_FOR_TABLE_ALIAS)?;
        Ok(TableFactor::Derived {
            lateral: match lateral {
                Lateral => true,
                NotLateral => false,
            },
            subquery,
            alias,
        })
    }

    fn parse_join_constraint(&mut self, natural: bool) -> Result<JoinConstraint, ParserError> {
        if natural {
            Ok(JoinConstraint::Natural)
        } else if self.parse_keyword(Keyword::ON) {
            let constraint = self.parse_expr()?;
            Ok(JoinConstraint::On(constraint))
        } else if self.parse_keyword(Keyword::USING) {
            let columns = self.parse_parenthesized_column_list(Mandatory)?;
            Ok(JoinConstraint::Using(columns))
        } else {
            Ok(JoinConstraint::None)
            // self.expected("ON, or USING after JOIN", self.peek_token())
        }
    }

    /// Parse a GRANT statement.
    pub fn parse_grant(&mut self) -> Result<Statement, ParserError> {
        let (privileges, objects) = self.parse_grant_revoke_privileges_objects()?;

        self.expect_keyword(Keyword::TO)?;
        let grantees = self.parse_comma_separated(Parser::parse_identifier)?;

        let with_grant_option =
            self.parse_keywords(&[Keyword::WITH, Keyword::GRANT, Keyword::OPTION]);

        let granted_by = self
            .parse_keywords(&[Keyword::GRANTED, Keyword::BY])
            .then(|| self.parse_identifier().unwrap());

        Ok(Statement::Grant {
            privileges,
            objects,
            grantees,
            with_grant_option,
            granted_by,
        })
    }

    fn parse_grant_revoke_privileges_objects(
        &mut self,
    ) -> Result<(Privileges, GrantObjects), ParserError> {
        let privileges = if self.parse_keyword(Keyword::ALL) {
            Privileges::All {
                with_privileges_keyword: self.parse_keyword(Keyword::PRIVILEGES),
            }
        } else {
            Privileges::Actions(
                self.parse_comma_separated(Parser::parse_grant_permission)?
                    .into_iter()
                    .map(|(kw, columns)| match kw {
                        Keyword::CONNECT => Action::Connect,
                        Keyword::CREATE => Action::Create,
                        Keyword::DELETE => Action::Delete,
                        Keyword::EXECUTE => Action::Execute,
                        Keyword::INSERT => Action::Insert { columns },
                        Keyword::REFERENCES => Action::References { columns },
                        Keyword::SELECT => Action::Select { columns },
                        Keyword::TEMPORARY => Action::Temporary,
                        Keyword::TRIGGER => Action::Trigger,
                        Keyword::TRUNCATE => Action::Truncate,
                        Keyword::UPDATE => Action::Update { columns },
                        Keyword::USAGE => Action::Usage,
                        _ => unreachable!(),
                    })
                    .collect(),
            )
        };

        self.expect_keyword(Keyword::ON)?;

        let objects = if self.parse_keywords(&[
            Keyword::ALL,
            Keyword::TABLES,
            Keyword::IN,
            Keyword::SCHEMA,
        ]) {
            GrantObjects::AllTablesInSchema {
                schemas: self.parse_comma_separated(Parser::parse_object_name)?,
            }
        } else if self.parse_keywords(&[
            Keyword::ALL,
            Keyword::SEQUENCES,
            Keyword::IN,
            Keyword::SCHEMA,
        ]) {
            GrantObjects::AllSequencesInSchema {
                schemas: self.parse_comma_separated(Parser::parse_object_name)?,
            }
        } else if self.parse_keywords(&[
            Keyword::ALL,
            Keyword::SOURCES,
            Keyword::IN,
            Keyword::SCHEMA,
        ]) {
            GrantObjects::AllSourcesInSchema {
                schemas: self.parse_comma_separated(Parser::parse_object_name)?,
            }
        } else if self.parse_keywords(&[
            Keyword::ALL,
            Keyword::MATERIALIZED,
            Keyword::VIEWS,
            Keyword::IN,
            Keyword::SCHEMA,
        ]) {
            GrantObjects::AllMviewsInSchema {
                schemas: self.parse_comma_separated(Parser::parse_object_name)?,
            }
        } else if self.parse_keywords(&[Keyword::MATERIALIZED, Keyword::VIEW]) {
            GrantObjects::Mviews(self.parse_comma_separated(Parser::parse_object_name)?)
        } else {
            let object_type = self.parse_one_of_keywords(&[
                Keyword::SEQUENCE,
                Keyword::DATABASE,
                Keyword::SCHEMA,
                Keyword::TABLE,
                Keyword::SOURCE,
            ]);
            let objects = self.parse_comma_separated(Parser::parse_object_name);
            match object_type {
                Some(Keyword::DATABASE) => GrantObjects::Databases(objects?),
                Some(Keyword::SCHEMA) => GrantObjects::Schemas(objects?),
                Some(Keyword::SEQUENCE) => GrantObjects::Sequences(objects?),
                Some(Keyword::SOURCE) => GrantObjects::Sources(objects?),
                Some(Keyword::TABLE) | None => GrantObjects::Tables(objects?),
                _ => unreachable!(),
            }
        };

        Ok((privileges, objects))
    }

    fn parse_grant_permission(&mut self) -> Result<(Keyword, Option<Vec<Ident>>), ParserError> {
        let kw = self.expect_one_of_keywords(&[
            Keyword::CONNECT,
            Keyword::CREATE,
            Keyword::DELETE,
            Keyword::EXECUTE,
            Keyword::INSERT,
            Keyword::REFERENCES,
            Keyword::SELECT,
            Keyword::TEMPORARY,
            Keyword::TRIGGER,
            Keyword::TRUNCATE,
            Keyword::UPDATE,
            Keyword::USAGE,
        ])?;
        let columns = match kw {
            Keyword::INSERT | Keyword::REFERENCES | Keyword::SELECT | Keyword::UPDATE => {
                let columns = self.parse_parenthesized_column_list(Optional)?;
                if columns.is_empty() {
                    None
                } else {
                    Some(columns)
                }
            }
            _ => None,
        };
        Ok((kw, columns))
    }

    /// Parse a REVOKE statement
    pub fn parse_revoke(&mut self) -> Result<Statement, ParserError> {
        let revoke_grant_option =
            self.parse_keywords(&[Keyword::GRANT, Keyword::OPTION, Keyword::FOR]);
        let (privileges, objects) = self.parse_grant_revoke_privileges_objects()?;

        self.expect_keyword(Keyword::FROM)?;
        let grantees = self.parse_comma_separated(Parser::parse_identifier)?;

        let granted_by = self
            .parse_keywords(&[Keyword::GRANTED, Keyword::BY])
            .then(|| self.parse_identifier().unwrap());

        let cascade = self.parse_keyword(Keyword::CASCADE);
        let restrict = self.parse_keyword(Keyword::RESTRICT);
        if cascade && restrict {
            return parser_err!("Cannot specify both CASCADE and RESTRICT in REVOKE");
        }

        Ok(Statement::Revoke {
            privileges,
            objects,
            grantees,
            granted_by,
            revoke_grant_option,
            cascade,
        })
    }

    /// Parse an INSERT statement
    pub fn parse_insert(&mut self) -> Result<Statement, ParserError> {
        self.expect_keyword(Keyword::INTO)?;

        let table_name = self.parse_object_name()?;
        let columns = self.parse_parenthesized_column_list(Optional)?;

        let source = Box::new(self.parse_query()?);
        let returning = self.parse_returning(Optional)?;

        Ok(Statement::Insert {
            table_name,
            columns,
            source,
            returning,
        })
    }

    pub fn parse_update(&mut self) -> Result<Statement, ParserError> {
        let table_name = self.parse_object_name()?;

        self.expect_keyword(Keyword::SET)?;
        let assignments = self.parse_comma_separated(Parser::parse_assignment)?;
        let selection = if self.parse_keyword(Keyword::WHERE) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let returning = self.parse_returning(Optional)?;
        Ok(Statement::Update {
            table_name,
            assignments,
            selection,
            returning,
        })
    }

    /// Parse a `var = expr` assignment, used in an UPDATE statement
    pub fn parse_assignment(&mut self) -> Result<Assignment, ParserError> {
        let id = self.parse_identifiers_non_keywords()?;
        self.expect_token(&Token::Eq)?;

        let value = if self.parse_keyword(Keyword::DEFAULT) {
            AssignmentValue::Default
        } else {
            AssignmentValue::Expr(self.parse_expr()?)
        };

        Ok(Assignment { id, value })
    }

    /// Parse a `[VARIADIC] name => expr`.
    fn parse_function_args(&mut self) -> Result<(bool, FunctionArg), ParserError> {
        let variadic = self.parse_keyword(Keyword::VARIADIC);
        let arg = if self.peek_nth_token(1) == Token::RArrow {
            let name = self.parse_identifier()?;

            self.expect_token(&Token::RArrow)?;
            let arg = self.parse_wildcard_or_expr()?.into();

            FunctionArg::Named { name, arg }
        } else {
            FunctionArg::Unnamed(self.parse_wildcard_or_expr()?.into())
        };
        Ok((variadic, arg))
    }

    pub fn parse_optional_args(
        &mut self,
    ) -> Result<(Vec<FunctionArg>, Vec<OrderByExpr>, bool), ParserError> {
        if self.consume_token(&Token::RParen) {
            Ok((vec![], vec![], false))
        } else {
            let args = self.parse_comma_separated(Parser::parse_function_args)?;
            if args
                .iter()
                .take(args.len() - 1)
                .any(|(variadic, _)| *variadic)
            {
                return parser_err!("VARIADIC argument must be last");
            }
            let variadic = args.last().map(|(variadic, _)| *variadic).unwrap_or(false);
            let args = args.into_iter().map(|(_, arg)| arg).collect();

            let order_by = if self.parse_keywords(&[Keyword::ORDER, Keyword::BY]) {
                self.parse_comma_separated(Parser::parse_order_by_expr)?
            } else {
                vec![]
            };
            self.expect_token(&Token::RParen)?;
            Ok((args, order_by, variadic))
        }
    }

    /// Parse a comma-delimited list of projections after SELECT
    pub fn parse_select_item(&mut self) -> Result<SelectItem, ParserError> {
        match self.parse_wildcard_or_expr()? {
            WildcardOrExpr::Expr(expr) => self
                .parse_optional_alias(keywords::RESERVED_FOR_COLUMN_ALIAS)
                .map(|alias| match alias {
                    Some(alias) => SelectItem::ExprWithAlias { expr, alias },
                    None => SelectItem::UnnamedExpr(expr),
                }),
            WildcardOrExpr::QualifiedWildcard(prefix, except) => {
                Ok(SelectItem::QualifiedWildcard(prefix, except))
            }
            WildcardOrExpr::ExprQualifiedWildcard(expr, prefix) => {
                Ok(SelectItem::ExprQualifiedWildcard(expr, prefix))
            }
            WildcardOrExpr::Wildcard(except) => Ok(SelectItem::Wildcard(except)),
        }
    }

    /// Parse an expression, optionally followed by ASC or DESC (used in ORDER BY)
    pub fn parse_order_by_expr(&mut self) -> Result<OrderByExpr, ParserError> {
        let expr = self.parse_expr()?;

        let asc = if self.parse_keyword(Keyword::ASC) {
            Some(true)
        } else if self.parse_keyword(Keyword::DESC) {
            Some(false)
        } else {
            None
        };

        let nulls_first = if self.parse_keywords(&[Keyword::NULLS, Keyword::FIRST]) {
            Some(true)
        } else if self.parse_keywords(&[Keyword::NULLS, Keyword::LAST]) {
            Some(false)
        } else {
            None
        };

        Ok(OrderByExpr {
            expr,
            asc,
            nulls_first,
        })
    }

    /// Parse a LIMIT clause
    pub fn parse_limit(&mut self) -> Result<Option<String>, ParserError> {
        if self.parse_keyword(Keyword::ALL) {
            Ok(None)
        } else {
            let number = self.parse_number_value()?;
            // TODO(Kexiang): support LIMIT expr
            if self.consume_token(&Token::DoubleColon) {
                self.expect_keyword(Keyword::BIGINT)?
            }
            Ok(Some(number))
        }
    }

    /// Parse an OFFSET clause
    pub fn parse_offset(&mut self) -> Result<String, ParserError> {
        let value = self.parse_number_value()?;
        // TODO(Kexiang): support LIMIT expr
        if self.consume_token(&Token::DoubleColon) {
            self.expect_keyword(Keyword::BIGINT)?;
        }
        _ = self.parse_one_of_keywords(&[Keyword::ROW, Keyword::ROWS]);
        Ok(value)
    }

    /// Parse a FETCH clause
    pub fn parse_fetch(&mut self) -> Result<Fetch, ParserError> {
        self.expect_one_of_keywords(&[Keyword::FIRST, Keyword::NEXT])?;
        let quantity = if self
            .parse_one_of_keywords(&[Keyword::ROW, Keyword::ROWS])
            .is_some()
        {
            None
        } else {
            let quantity = self.parse_number_value()?;
            self.expect_one_of_keywords(&[Keyword::ROW, Keyword::ROWS])?;
            Some(quantity)
        };
        let with_ties = if self.parse_keyword(Keyword::ONLY) {
            false
        } else if self.parse_keywords(&[Keyword::WITH, Keyword::TIES]) {
            true
        } else {
            return self.expected("one of ONLY or WITH TIES", self.peek_token());
        };
        Ok(Fetch {
            with_ties,
            quantity,
        })
    }

    pub fn parse_values(&mut self) -> Result<Values, ParserError> {
        let values = self.parse_comma_separated(|parser| {
            parser.expect_token(&Token::LParen)?;
            let exprs = parser.parse_comma_separated(Parser::parse_expr)?;
            parser.expect_token(&Token::RParen)?;
            Ok(exprs)
        })?;
        Ok(Values(values))
    }

    pub fn parse_start_transaction(&mut self) -> Result<Statement, ParserError> {
        self.expect_keyword(Keyword::TRANSACTION)?;
        Ok(Statement::StartTransaction {
            modes: self.parse_transaction_modes()?,
        })
    }

    pub fn parse_begin(&mut self) -> Result<Statement, ParserError> {
        let _ = self.parse_one_of_keywords(&[Keyword::TRANSACTION, Keyword::WORK]);
        Ok(Statement::Begin {
            modes: self.parse_transaction_modes()?,
        })
    }

    pub fn parse_transaction_modes(&mut self) -> Result<Vec<TransactionMode>, ParserError> {
        let mut modes = vec![];
        let mut required = false;
        loop {
            let mode = if self.parse_keywords(&[Keyword::ISOLATION, Keyword::LEVEL]) {
                let iso_level = if self.parse_keywords(&[Keyword::READ, Keyword::UNCOMMITTED]) {
                    TransactionIsolationLevel::ReadUncommitted
                } else if self.parse_keywords(&[Keyword::READ, Keyword::COMMITTED]) {
                    TransactionIsolationLevel::ReadCommitted
                } else if self.parse_keywords(&[Keyword::REPEATABLE, Keyword::READ]) {
                    TransactionIsolationLevel::RepeatableRead
                } else if self.parse_keyword(Keyword::SERIALIZABLE) {
                    TransactionIsolationLevel::Serializable
                } else {
                    self.expected("isolation level", self.peek_token())?
                };
                TransactionMode::IsolationLevel(iso_level)
            } else if self.parse_keywords(&[Keyword::READ, Keyword::ONLY]) {
                TransactionMode::AccessMode(TransactionAccessMode::ReadOnly)
            } else if self.parse_keywords(&[Keyword::READ, Keyword::WRITE]) {
                TransactionMode::AccessMode(TransactionAccessMode::ReadWrite)
            } else if required {
                self.expected("transaction mode", self.peek_token())?
            } else {
                break;
            };
            modes.push(mode);
            // ANSI requires a comma after each transaction mode, but
            // PostgreSQL, for historical reasons, does not. We follow
            // PostgreSQL in making the comma optional, since that is strictly
            // more general.
            required = self.consume_token(&Token::Comma);
        }
        Ok(modes)
    }

    pub fn parse_commit(&mut self) -> Result<Statement, ParserError> {
        Ok(Statement::Commit {
            chain: self.parse_commit_rollback_chain()?,
        })
    }

    pub fn parse_rollback(&mut self) -> Result<Statement, ParserError> {
        Ok(Statement::Rollback {
            chain: self.parse_commit_rollback_chain()?,
        })
    }

    pub fn parse_commit_rollback_chain(&mut self) -> Result<bool, ParserError> {
        let _ = self.parse_one_of_keywords(&[Keyword::TRANSACTION, Keyword::WORK]);
        if self.parse_keyword(Keyword::AND) {
            let chain = !self.parse_keyword(Keyword::NO);
            self.expect_keyword(Keyword::CHAIN)?;
            Ok(chain)
        } else {
            Ok(false)
        }
    }

    fn parse_deallocate(&mut self) -> Result<Statement, ParserError> {
        let prepare = self.parse_keyword(Keyword::PREPARE);
        let name = self.parse_identifier()?;
        Ok(Statement::Deallocate { name, prepare })
    }

    fn parse_execute(&mut self) -> Result<Statement, ParserError> {
        let name = self.parse_identifier()?;

        let mut parameters = vec![];
        if self.consume_token(&Token::LParen) {
            parameters = self.parse_comma_separated(Parser::parse_expr)?;
            self.expect_token(&Token::RParen)?;
        }

        Ok(Statement::Execute { name, parameters })
    }

    fn parse_prepare(&mut self) -> Result<Statement, ParserError> {
        let name = self.parse_identifier()?;

        let mut data_types = vec![];
        if self.consume_token(&Token::LParen) {
            data_types = self.parse_comma_separated(Parser::parse_data_type)?;
            self.expect_token(&Token::RParen)?;
        }

        self.expect_keyword(Keyword::AS)?;
        let statement = Box::new(self.parse_statement()?);
        Ok(Statement::Prepare {
            name,
            data_types,
            statement,
        })
    }

    fn parse_comment(&mut self) -> Result<Statement, ParserError> {
        self.expect_keyword(Keyword::ON)?;
        let token = self.next_token();

        let (object_type, object_name) = match token.token {
            Token::Word(w) if w.keyword == Keyword::COLUMN => {
                let object_name = self.parse_object_name()?;
                (CommentObject::Column, object_name)
            }
            Token::Word(w) if w.keyword == Keyword::TABLE => {
                let object_name = self.parse_object_name()?;
                (CommentObject::Table, object_name)
            }
            _ => self.expected("comment object_type", token)?,
        };

        self.expect_keyword(Keyword::IS)?;
        let comment = if self.parse_keyword(Keyword::NULL) {
            None
        } else {
            Some(self.parse_literal_string()?)
        };
        Ok(Statement::Comment {
            object_type,
            object_name,
            comment,
        })
    }
}

impl Word {
    /// Convert a Word to a Identifier, return ParserError when the Word's value is a empty string.
    pub fn to_ident(&self) -> Result<Ident, ParserError> {
        if self.value.is_empty() {
            parser_err!(format!(
                "zero-length delimited identifier at or near \"{self}\""
            ))
        } else {
            Ok(Ident {
                value: self.value.clone(),
                quote_style: self.quote_style,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::run_parser_method;

    #[test]
    fn test_prev_index() {
        let sql = "SELECT version";
        run_parser_method(sql, |parser| {
            assert_eq!(parser.peek_token(), Token::make_keyword("SELECT"));
            assert_eq!(parser.next_token(), Token::make_keyword("SELECT"));
            parser.prev_token();
            assert_eq!(parser.next_token(), Token::make_keyword("SELECT"));
            assert_eq!(parser.next_token(), Token::make_word("version", None));
            parser.prev_token();
            assert_eq!(parser.peek_token(), Token::make_word("version", None));
            assert_eq!(parser.next_token(), Token::make_word("version", None));
            assert_eq!(parser.peek_token(), Token::EOF);
            parser.prev_token();
            assert_eq!(parser.next_token(), Token::make_word("version", None));
            assert_eq!(parser.next_token(), Token::EOF);
            assert_eq!(parser.next_token(), Token::EOF);
            parser.prev_token();
        });
    }

    #[test]
    fn test_parse_integer_min() {
        let min_bigint = "-9223372036854775808";
        run_parser_method(min_bigint, |parser| {
            assert_eq!(
                parser.parse_expr().unwrap(),
                Expr::Value(Value::Number("-9223372036854775808".to_string()))
            )
        });
    }
}
