//! Async arrow function parsing.
//!
//! More information:
//!  - [MDN documentation][mdn]
//!  - [ECMAScript specification][spec]
//!
//! [mdn]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Functions/Arrow_functions
//! [spec]: https://tc39.es/ecma262/#sec-async-arrow-function-definitions

use super::arrow_function::ExpressionBody;
use crate::{
    error::{Error, ErrorContext, ParseResult},
    lexer::{Error as LexError, TokenKind},
    parser::{
        AllowIn, AllowYield, Cursor, OrAbrupt, TokenParser,
        expression::{BindingIdentifier, primary::expression_to_formal_parameters},
        function::{FormalParameters, FunctionBody},
        name_in_lexically_declared_names,
    },
    source::ReadChar,
};
use ast::{
    Keyword,
    operations::{ContainsSymbol, bound_names, contains, lexically_declared_names},
};
use boa_ast::{
    self as ast, LinearSpan, Punctuator, Span, Spanned, StatementList,
    declaration::Variable,
    expression::{Call, Expression, Identifier, literal::ObjectMethodDefinition},
    function::{
        ArrowFunction as AstArrowFunction, AsyncArrowFunction as AstAsyncArrowFunction,
        AsyncFunctionDeclaration, AsyncFunctionExpression, AsyncGeneratorDeclaration,
        AsyncGeneratorExpression, ClassDeclaration, ClassExpression, FormalParameter,
        FormalParameterList, FunctionDeclaration, FunctionExpression, GeneratorDeclaration,
        GeneratorExpression,
    },
    statement::Return,
    visitor::{VisitWith, Visitor},
};
use boa_interner::{Interner, Sym};
use core::ops::ControlFlow;

fn contains_await_identifier(parameters: &FormalParameterList) -> bool {
    struct AwaitIdentifierVisitor;

    impl<'ast> Visitor<'ast> for AwaitIdentifierVisitor {
        type BreakTy = ();

        fn visit_identifier(&mut self, identifier: &'ast Identifier) -> ControlFlow<()> {
            if identifier.sym() == Sym::AWAIT {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        }

        fn visit_arrow_function(&mut self, function: &'ast AstArrowFunction) -> ControlFlow<()> {
            function.parameters().visit_with(self)
        }

        fn visit_async_arrow_function(
            &mut self,
            function: &'ast AstAsyncArrowFunction,
        ) -> ControlFlow<()> {
            function.parameters().visit_with(self)
        }

        fn visit_function_expression(&mut self, _: &'ast FunctionExpression) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }

        fn visit_function_declaration(&mut self, _: &'ast FunctionDeclaration) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }

        fn visit_async_function_expression(
            &mut self,
            _: &'ast AsyncFunctionExpression,
        ) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }

        fn visit_async_function_declaration(
            &mut self,
            _: &'ast AsyncFunctionDeclaration,
        ) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }

        fn visit_generator_expression(&mut self, _: &'ast GeneratorExpression) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }

        fn visit_generator_declaration(
            &mut self,
            _: &'ast GeneratorDeclaration,
        ) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }

        fn visit_async_generator_expression(
            &mut self,
            _: &'ast AsyncGeneratorExpression,
        ) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }

        fn visit_async_generator_declaration(
            &mut self,
            _: &'ast AsyncGeneratorDeclaration,
        ) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }

        fn visit_class_expression(&mut self, _: &'ast ClassExpression) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }

        fn visit_class_declaration(&mut self, _: &'ast ClassDeclaration) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }

        fn visit_object_method_definition(
            &mut self,
            _: &'ast ObjectMethodDefinition,
        ) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }
    }

    parameters
        .visit_with(&mut AwaitIdentifierVisitor)
        .is_break()
}

/// Async arrow function parsing.
///
/// More information:
///  - [MDN documentation][mdn]
///  - [ECMAScript specification][spec]
///
/// [mdn]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Functions/Arrow_functions
/// [spec]: https://tc39.es/ecma262/#prod-AsyncArrowFunction
#[derive(Debug, Clone, Copy)]
pub(in crate::parser) struct AsyncArrowFunction {
    allow_in: AllowIn,
    allow_yield: AllowYield,
}

impl AsyncArrowFunction {
    /// Creates a new `AsyncArrowFunction` parser.
    pub(in crate::parser) fn new<I, Y>(allow_in: I, allow_yield: Y) -> Self
    where
        I: Into<AllowIn>,
        Y: Into<AllowYield>,
    {
        Self {
            allow_in: allow_in.into(),
            allow_yield: allow_yield.into(),
        }
    }

    /// Reinterprets a parsed `async(...)` call as an async arrow head when it is followed by `=>`.
    pub(super) fn parse_from_call<R>(
        self,
        call: &Call,
        start_linear_span: LinearSpan,
        async_token_span: Span,
        cursor: &mut Cursor<R>,
        interner: &mut Interner,
    ) -> ParseResult<ast::function::AsyncArrowFunction>
    where
        R: ReadChar,
    {
        let mut parameters = Vec::new();
        let strict = cursor.strict();
        for (index, argument) in call.args().iter().enumerate() {
            if let Expression::Spread(spread) = argument {
                if index + 1 != call.args().len() || call.has_trailing_comma() {
                    return Err(Error::general(
                        "rest parameter must be last formal parameter",
                        spread.span().start(),
                    ));
                }

                let previous_len = parameters.len();
                expression_to_formal_parameters(
                    spread.target(),
                    &mut parameters,
                    strict,
                    spread.span(),
                )?;
                if parameters.len() != previous_len + 1 {
                    return Err(Error::general(
                        "invalid rest parameter",
                        spread.span().start(),
                    ));
                }
                let parameter = parameters.pop().expect("one parameter checked above");
                if parameter.init().is_some() {
                    return Err(Error::general(
                        "rest parameter cannot have a default initializer",
                        spread.span().start(),
                    ));
                }
                parameters.push(FormalParameter::new(parameter.variable().clone(), true));
            } else {
                expression_to_formal_parameters(
                    argument,
                    &mut parameters,
                    strict,
                    argument.span(),
                )?;
            }
        }

        self.finish(
            FormalParameterList::from(parameters),
            call.span().start(),
            start_linear_span,
            async_token_span,
            cursor,
            interner,
        )
    }

    fn finish<R>(
        self,
        params: FormalParameterList,
        params_start_position: ast::Position,
        start_linear_span: LinearSpan,
        async_token_span: Span,
        cursor: &mut Cursor<R>,
        interner: &mut Interner,
    ) -> ParseResult<ast::function::AsyncArrowFunction>
    where
        R: ReadChar,
    {
        cursor.peek_expect_no_lineterminator(0, "async arrow function", interner)?;
        cursor.expect(Punctuator::Arrow, "async arrow function", interner)?;

        let body = AsyncConciseBody::new(self.allow_in).parse(cursor, interner)?;

        if params.has_duplicates() {
            return Err(Error::lex(LexError::Syntax(
                "Duplicate parameter name not allowed in this context".into(),
                params_start_position,
            )));
        }

        if contains(&params, ContainsSymbol::YieldExpression) {
            return Err(Error::lex(LexError::Syntax(
                "Yield expression not allowed in this context".into(),
                params_start_position,
            )));
        }

        if contains_await_identifier(&params) || contains(&params, ContainsSymbol::AwaitExpression)
        {
            return Err(Error::lex(LexError::Syntax(
                "Await expression not allowed in this context".into(),
                params_start_position,
            )));
        }

        if body.strict() && !params.is_simple() {
            return Err(Error::lex(LexError::Syntax(
                "Illegal 'use strict' directive in function with non-simple parameter list".into(),
                params_start_position,
            )));
        }

        name_in_lexically_declared_names(
            &bound_names(&params),
            &lexically_declared_names(&body),
            params_start_position,
            interner,
        )?;

        let linear_pos_end = body.linear_pos_end();
        let linear_span = start_linear_span.union(linear_pos_end);
        let body_span_end = body.span().end();
        Ok(ast::function::AsyncArrowFunction::new(
            None,
            params,
            body,
            linear_span,
            Span::new(async_token_span.start(), body_span_end),
        ))
    }
}

impl<R> TokenParser<R> for AsyncArrowFunction
where
    R: ReadChar,
{
    type Output = ast::function::AsyncArrowFunction;

    fn parse(self, cursor: &mut Cursor<R>, interner: &mut Interner) -> ParseResult<Self::Output> {
        let async_token =
            cursor.expect((Keyword::Async, false), "async arrow function", interner)?;
        let start_linear_span = async_token.linear_span();
        let async_token_span = async_token.span();
        cursor.peek_expect_no_lineterminator(0, "async arrow function", interner)?;

        let next_token = cursor.peek(0, interner).or_abrupt()?;
        let (params, params_start_position) =
            if next_token.kind() == &TokenKind::Punctuator(Punctuator::OpenParen) {
                let params_start_position = cursor
                    .expect(Punctuator::OpenParen, "async arrow function", interner)?
                    .span()
                    .end();

                let params = FormalParameters::new(false, true).parse(cursor, interner)?;
                cursor.expect(Punctuator::CloseParen, "async arrow function", interner)?;
                (params, params_start_position)
            } else {
                let params_start_position = next_token.span().start();
                let param = BindingIdentifier::new(self.allow_yield, true)
                    .parse(cursor, interner)
                    .set_context("async arrow function")?;
                (
                    FormalParameterList::from(FormalParameter::new(
                        Variable::from_identifier(param, None),
                        false,
                    )),
                    params_start_position,
                )
            };

        self.finish(
            params,
            params_start_position,
            start_linear_span,
            async_token_span,
            cursor,
            interner,
        )
    }
}

/// <https://tc39.es/ecma262/#prod-AsyncConciseBody>
#[derive(Debug, Clone, Copy)]
pub(in crate::parser) struct AsyncConciseBody {
    allow_in: AllowIn,
}

impl AsyncConciseBody {
    /// Creates a new `AsyncConciseBody` parser.
    pub(in crate::parser) fn new<I>(allow_in: I) -> Self
    where
        I: Into<AllowIn>,
    {
        Self {
            allow_in: allow_in.into(),
        }
    }
}

impl<R> TokenParser<R> for AsyncConciseBody
where
    R: ReadChar,
{
    type Output = ast::function::FunctionBody;

    fn parse(self, cursor: &mut Cursor<R>, interner: &mut Interner) -> ParseResult<Self::Output> {
        let body = if let TokenKind::Punctuator(Punctuator::OpenBlock) =
            cursor.peek(0, interner).or_abrupt()?.kind()
        {
            FunctionBody::new(false, true, "async arrow function").parse(cursor, interner)?
        } else {
            let expression = ExpressionBody::new(self.allow_in, true).parse(cursor, interner)?;
            let span = expression.span();
            ast::function::FunctionBody::new(
                StatementList::new(
                    [ast::Statement::Return(Return::new(expression.into())).into()],
                    cursor.linear_pos(),
                    false,
                ),
                span,
            )
        };

        Ok(body)
    }
}
