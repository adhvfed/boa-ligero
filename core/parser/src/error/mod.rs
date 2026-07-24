//! Error and result implementation for the parser.

#[cfg(test)]
mod tests;

use crate::lexer::Error as LexError;
use boa_ast::{Position, Span};
use std::fmt;

/// Result of a parsing operation.
pub type ParseResult<T> = Result<T, Error>;

/// Details for an error caused by encountering an unexpected token.
#[derive(Debug)]
pub struct ExpectedError {
    expected: Box<[String]>,
    found: Box<str>,
    context: &'static str,
    span: Span,
}

impl ExpectedError {
    /// Returns the token descriptions that the parser expected.
    #[must_use]
    pub fn expected(&self) -> &[String] {
        &self.expected
    }

    /// Returns the token description that the parser found.
    #[must_use]
    pub fn found(&self) -> &str {
        &self.found
    }

    /// Returns the parsing context for the error.
    #[must_use]
    pub const fn context(&self) -> &'static str {
        self.context
    }

    /// Returns the source span of the unexpected token.
    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

/// Details for an error caused by an invalid token in a parsing context.
#[derive(Debug)]
pub struct UnexpectedError {
    message: Box<str>,
    found: Box<str>,
    span: Span,
}

impl UnexpectedError {
    /// Returns why the token is invalid in this parsing context.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the token description that the parser found.
    #[must_use]
    pub fn found(&self) -> &str {
        &self.found
    }

    /// Returns the source span of the unexpected token.
    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

/// Adds context to a parser error.
pub(crate) trait ErrorContext {
    /// Sets the context of the error, if possible.
    fn set_context(self, context: &'static str) -> Self;

    /// Gets the context of the error, if any.
    fn context(&self) -> Option<&'static str>;
}

impl<T> ErrorContext for ParseResult<T> {
    fn set_context(self, context: &'static str) -> Self {
        self.map_err(|e| e.set_context(context))
    }

    fn context(&self) -> Option<&'static str> {
        self.as_ref().err().and_then(ErrorContext::context)
    }
}

impl ErrorContext for Error {
    fn set_context(self, new_context: &'static str) -> Self {
        match self {
            Self::Expected(mut error) => {
                error.context = new_context;
                Self::Expected(error)
            }
            e => e,
        }
    }

    fn context(&self) -> Option<&'static str> {
        if let Self::Expected(error) = self {
            Some(error.context)
        } else {
            None
        }
    }
}

impl From<LexError> for Error {
    #[inline]
    fn from(e: LexError) -> Self {
        Self::lex(e)
    }
}

/// An enum which represents errors encountered during parsing an expression
#[derive(Debug)]
pub enum Error {
    /// When it expected a certain kind of token, but got another as part of something
    Expected(Box<ExpectedError>),

    /// When a token is unexpected
    Unexpected(Box<UnexpectedError>),

    /// When there is an abrupt end to the parsing
    AbruptEnd,

    /// A lexing error.
    Lex {
        /// The error that occurred during lexing.
        err: LexError,
    },

    /// A scope analysis error.
    ScopeAnalysis {
        /// The error that occurred during scope analysis.
        err: &'static str,
    },

    /// Catch all General Error
    General {
        /// The error message.
        message: Box<str>,

        /// Position of the source code where the error occurred.
        position: Position,
    },
}

impl Error {
    /// Creates an `Expected` parsing error.
    pub(crate) fn expected<E, F>(expected: E, found: F, span: Span, context: &'static str) -> Self
    where
        E: Into<Box<[String]>>,
        F: Into<Box<str>>,
    {
        let expected = expected.into();
        debug_assert_ne!(expected.len(), 0);

        Self::Expected(Box::new(ExpectedError {
            expected,
            found: found.into(),
            span,
            context,
        }))
    }

    /// Creates an `Unexpected` parsing error.
    pub(crate) fn unexpected<F, C>(found: F, span: Span, message: C) -> Self
    where
        F: Into<Box<str>>,
        C: Into<Box<str>>,
    {
        Self::Unexpected(Box::new(UnexpectedError {
            found: found.into(),
            span,
            message: message.into(),
        }))
    }

    /// Creates a `ScopeAnalysis` parsing error.
    pub(crate) fn scope_analysis(err: &'static str) -> Self {
        Self::ScopeAnalysis { err }
    }

    /// Creates a "general" parsing error.
    pub(crate) fn general<S, P>(message: S, position: P) -> Self
    where
        S: Into<Box<str>>,
        P: Into<Position>,
    {
        Self::General {
            message: message.into(),
            position: position.into(),
        }
    }

    /// Creates a "general" parsing error with the specific error message for a misplaced function declaration.
    pub(crate) fn misplaced_function_declaration(position: Position, strict: bool) -> Self {
        Self::General {
            message: format!(
                "{}functions can only be declared at the top level or inside a block.",
                if strict { "in strict mode code, " } else { "" }
            )
            .into(),
            position,
        }
    }

    /// Creates a "general" parsing error with the specific error message for a wrong function declaration with label.
    pub(crate) fn wrong_labelled_function_declaration(position: Position) -> Self {
        Self::General {
            message: "labelled functions can only be declared at the top level or inside a block"
                .into(),
            position,
        }
    }

    /// Creates a parsing error from a lexing error.
    pub(crate) const fn lex(e: LexError) -> Self {
        Self::Lex { err: e }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Expected(error) => {
                write!(f, "expected ")?;
                match &*error.expected {
                    [single] => write!(f, "token '{single}'")?,
                    expected => {
                        write!(f, "one of ")?;
                        for (i, token) in expected.iter().enumerate() {
                            let prefix = if i == 0 {
                                ""
                            } else if i == expected.len() - 1 {
                                " or "
                            } else {
                                ", "
                            };
                            write!(f, "{prefix}'{token}'")?;
                        }
                    }
                }
                if let Some(context) = self.context() {
                    write!(
                        f,
                        ", got '{}' in {context} at line {}, col {}",
                        error.found,
                        error.span.start().line_number(),
                        error.span.start().column_number()
                    )
                } else {
                    write!(
                        f,
                        ", got '{}' at line {}, col {}",
                        error.found,
                        error.span.start().line_number(),
                        error.span.start().column_number()
                    )
                }
            }
            Self::Unexpected(error) => write!(
                f,
                "unexpected token '{}', {} at line {}, col {}",
                error.found,
                error.message,
                error.span.start().line_number(),
                error.span.start().column_number()
            ),
            Self::AbruptEnd => f.write_str("abrupt end"),
            Self::General { message, position } => write!(
                f,
                "{message} at line {}, col {}",
                position.line_number(),
                position.column_number()
            ),
            Self::Lex { err } => err.fmt(f),
            Self::ScopeAnalysis { err } => write!(f, "invalid scope analysis: {err}"),
        }
    }
}

impl std::error::Error for Error {}
