//! Tokenization and parsing of source code into syntax trees.

use std::collections::HashMap;
use std::fmt;
use std::iter::Peekable;
use std::mem::swap;
use std::ops::Deref;

use crate::syntax::*;
use crate::func::{Scope, BodyTokens};
use crate::utility::{Splinor, Spline, Splined, StrExt};

use unicode_segmentation::{UnicodeSegmentation, UWordBounds};


/// An iterator over the tokens of source code.
#[derive(Clone)]
pub struct Tokens<'s> {
    source: &'s str,
    words: Peekable<UWordBounds<'s>>,
    state: TokensState<'s>,
    stack: Vec<TokensState<'s>>,
}

impl fmt::Debug for Tokens<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("source", &self.source)
            .field("words", &"Peekable<UWordBounds>")
            .field("state", &self.state)
            .field("stack", &self.stack)
            .finish()
    }
}

/// The state the tokenizer is in.
#[derive(Debug, Clone)]
enum TokensState<'s> {
    /// The base state if there is nothing special we are in.
    Body,
    /// Inside a function header. Here colons and equal signs get parsed
    /// as distinct tokens rather than text.
    Function,
    /// We expect either the end of the function or the beginning of the body.
    MaybeBody,
    /// We are inside one unicode word that consists of multiple tokens,
    /// because it contains double underscores.
    DoubleUnderscore(Spline<'s, Token<'s>>),
}

impl PartialEq for TokensState<'_> {
    fn eq(&self, other: &TokensState) -> bool {
        use TokensState as TS;

        match (self, other) {
            (TS::Body, TS::Body) => true,
            (TS::Function, TS::Function) => true,
            (TS::MaybeBody, TS::MaybeBody) => true,
            // They are not necessarily different, but we don't care
            _ => false,
        }
    }
}

impl<'s> Iterator for Tokens<'s> {
    type Item = Token<'s>;

    /// Advance the iterator, return the next token or nothing.
    fn next(&mut self) -> Option<Token<'s>> {
        use TokensState as TS;

        // Return the remaining words and double underscores.
        if let TS::DoubleUnderscore(splinor) = &mut self.state {
            loop {
                if let Some(splined) = splinor.next() {
                    return Some(match splined {
                        Splined::Value(word) if word != "" => Token::Word(word),
                        Splined::Splinor(s) => s,
                        _ => continue,
                    });
                } else {
                    self.unswitch();
                    break;
                }
            }
        }

        // Skip whitespace, but if at least one whitespace word existed,
        // remember that, because we return a space token.
        let mut whitespace = false;
        while let Some(word) = self.words.peek() {
            if !word.is_whitespace() {
                break;
            }
            whitespace = true;
            self.advance();
        }
        if whitespace {
            return Some(Token::Space);
        }

        // Function maybe has a body
        if self.state == TS::MaybeBody {
            match *self.words.peek()? {
                "[" => {
                    self.state = TS::Body;
                    return Some(self.consumed(Token::LeftBracket));
                },
                _ => self.unswitch(),
            }
        }

        // Now all special cases are handled and we can finally look at the
        // next words.
        let next = self.words.next()?;
        let afterwards = self.words.peek();

        Some(match next {
            // Special characters
            "[" => {
                self.switch(TS::Function);
                Token::LeftBracket
            },
            "]" => {
                if self.state == TS::Function {
                    self.state = TS::MaybeBody;
                }
                Token::RightBracket
            },
            "$" => Token::Dollar,
            "#" => Token::Hashtag,

            // Context sensitive operators
            ":" if self.state == TS::Function => Token::Colon,
            "=" if self.state == TS::Function => Token::Equals,

            // Double star/underscore
            "*" if afterwards == Some(&"*") => self.consumed(Token::DoubleStar),
            "__" => Token::DoubleUnderscore,

            // Newlines
            "\n" | "\r\n" => Token::Newline,

            // Escaping
            r"\" => {
                if let Some(next) = afterwards {
                    let escapable = match *next {
                        "[" | "]" | "$" | "#" | r"\" | ":" | "=" | "*" | "_" => true,
                        w if w.starts_with("__") => true,
                        _ => false,
                    };

                    if escapable {
                        let next = *next;
                        self.advance();
                        return Some(Token::Word(next));
                    }
                }

                Token::Word(r"\")
            },

            // Double underscores hidden in words.
            word if word.contains("__") => {
                let spline = word.spline("__", Token::DoubleUnderscore);
                self.switch(TS::DoubleUnderscore(spline));
                return self.next();
            },

            // Now it seems like it's just a normal word.
            word => Token::Word(word),
        })
    }
}

impl<'s> Tokens<'s> {
    /// Create a new token stream from text.
    #[inline]
    pub fn new(source: &'s str) -> Tokens<'s> {
        Tokens {
            source,
            words: source.split_word_bounds().peekable(),
            state: TokensState::Body,
            stack: vec![],
        }
    }

    /// Advance the iterator by one step.
    fn advance(&mut self) {
        self.words.next();
    }

    /// Switch to the given state.
    fn switch(&mut self, mut state: TokensState<'s>) {
        swap(&mut state, &mut self.state);
        self.stack.push(state);
    }

    /// Go back to the top-of-stack state.
    fn unswitch(&mut self) {
         self.state = self.stack.pop().unwrap_or(TokensState::Body);
    }

    /// Advance and return the given token.
    fn consumed(&mut self, token: Token<'s>) -> Token<'s> {
        self.advance();
        token
    }
}

/// Transforms token streams to syntax trees.
pub struct Parser<'s, T> where T: Iterator<Item=Token<'s>> {
    tokens: Peekable<T>,
    scope: ParserScope<'s>,
    state: ParserState,
    tree: SyntaxTree,
}

/// The state the parser is in.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum ParserState {
    /// The base state of the parser.
    Body,
    /// We saw one newline already.
    FirstNewline,
    /// We wrote a newline.
    WroteNewline,
    /// Inside a function header.
    Function,
}

impl<'s, T> Parser<'s, T> where T: Iterator<Item=Token<'s>> {
    /// Create a new parser from a type that emits results of tokens.
    pub fn new(tokens: T) -> Parser<'s, T> {
        Parser::new_internal(ParserScope::Owned(Scope::new()), tokens)
    }

    /// Create a new parser with a scope containing function definitions.
    pub fn with_scope(scope: &'s Scope, tokens: T) -> Parser<'s, T> {
        Parser::new_internal(ParserScope::Shared(scope), tokens)
    }

    /// Internal helper for construction.
    fn new_internal(scope: ParserScope<'s>, tokens: T) -> Parser<'s, T> {
        Parser {
            tokens: tokens.peekable(),
            scope,
            state: ParserState::Body,
            tree: SyntaxTree::new(),
        }
    }

    /// Parse into an abstract syntax tree.
    pub(crate) fn parse(mut self) -> ParseResult<SyntaxTree> {
        use ParserState as PS;

        while let Some(token) = self.tokens.peek() {
            let token = *token;

            // Skip over comments.
            if token == Token::Hashtag {
                self.skip_while(|&t| t != Token::Newline);
                self.advance();
            }

            // Handles all the states.
            match self.state {
                PS::FirstNewline => match token {
                    Token::Newline => {
                        self.append_consumed(Node::Newline);
                        self.switch(PS::WroteNewline);
                    },
                    Token::Space => self.append_space_consumed(),
                    _ => {
                        self.append_space();
                        self.switch(PS::Body);
                    },
                }

                PS::WroteNewline => match token {
                    Token::Newline | Token::Space => self.append_space_consumed(),
                    _ => self.switch(PS::Body),
                }

                PS::Body => match token {
                    // Whitespace
                    Token::Space => self.append_space_consumed(),
                    Token::Newline => self.switch_consumed(PS::FirstNewline),

                    // Words
                    Token::Word(word) => self.append_consumed(Node::Word(word.to_owned())),

                    // Functions
                    Token::LeftBracket => self.switch_consumed(PS::Function),
                    Token::RightBracket => {
                        return Err(ParseError::new("unexpected closing bracket"));
                    },

                    // Modifiers
                    Token::DoubleUnderscore => self.append_consumed(Node::ToggleItalics),
                    Token::DoubleStar => self.append_consumed(Node::ToggleBold),
                    Token::Dollar => self.append_consumed(Node::ToggleMath),

                    // Should not happen
                    Token::Colon | Token::Equals | Token::Hashtag => unreachable!(),
                },

                PS::Function => {
                    let name = if let Token::Word(word) = token {
                        match Ident::new(word) {
                            Some(ident) => ident,
                            None => return Err(ParseError::new("invalid identifier")),
                        }
                    } else {
                        return Err(ParseError::new("expected identifier"));
                    };
                    self.advance();

                    // Expect the header closing bracket.
                    if self.tokens.next() != Some(Token::RightBracket) {
                        return Err(ParseError::new("expected closing bracket"));
                    }

                    // Store the header information of the function invocation.
                    let header = FuncHeader {
                        name: name.clone(),
                        args: vec![],
                        kwargs: HashMap::new(),
                    };

                    // This function has a body.
                    let mut tokens = if let Some(Token::LeftBracket) = self.tokens.peek() {
                        self.advance();
                        Some(FuncTokens::new(&mut self.tokens))
                    } else {
                        None
                    };

                    // A mutably borrowed view over the tokens.
                    let borrow_tokens: BodyTokens<'_> = tokens.as_mut().map(|toks| {
                        Box::new(toks) as Box<dyn Iterator<Item=Token<'_>>>
                    });

                    // Run the parser over the tokens.
                    let body = if let Some(parser) = self.scope.get_parser(&name) {
                        parser(&header, borrow_tokens, &self.scope)?
                    } else {
                        return Err(ParseError::new(format!("unknown function: '{}'", &name)));
                    };

                    // Expect the closing bracket if it had a body.
                    if let Some(tokens) = tokens {
                        if tokens.unexpected_end {
                            return Err(ParseError::new("expected closing bracket"));
                        }
                    }

                    // Finally this function is parsed to the end.
                    self.append(Node::Func(FuncCall {
                        header,
                        body,
                    }));

                    self.switch(PS::Body);
                },

            }
        }

        Ok(self.tree)
    }

    /// Advance the iterator by one step.
    fn advance(&mut self) {
        self.tokens.next();
    }

    /// Append a node to the tree.
    fn append(&mut self, node: Node) {
        self.tree.nodes.push(node);
    }

    /// Advance and return the given node.
    fn append_consumed(&mut self, node: Node) { self.advance(); self.append(node); }

    /// Append a space if there is not one already.
    fn append_space(&mut self) {
        if self.last() != Some(&Node::Space) {
            self.append(Node::Space);
        }
    }

    /// Advance and append a space if there is not one already.
    fn append_space_consumed(&mut self) {
        self.advance();
        self.append_space();
    }

    /// Switch the state.
    fn switch(&mut self, state: ParserState) {
        self.state = state;
    }

    /// Advance and switch the state.
    fn switch_consumed(&mut self, state: ParserState) {
        self.advance();
        self.state = state;
    }

    /// The last appended node of the tree.
    fn last(&self) -> Option<&Node> {
        self.tree.nodes.last()
    }

    /// Skip tokens until the condition is met.
    fn skip_while<F>(&mut self, f: F) where F: Fn(&Token) -> bool {
        while let Some(token) = self.tokens.peek() {
            if !f(token) {
                break;
            }
            self.advance();
        }
    }
}

/// An owned or shared scope.
enum ParserScope<'s> {
    Owned(Scope),
    Shared(&'s Scope)
}

impl Deref for ParserScope<'_> {
    type Target = Scope;

    fn deref(&self) -> &Scope {
        match self {
            ParserScope::Owned(scope) => &scope,
            ParserScope::Shared(scope) => scope,
        }
    }
}

/// A token iterator that that stops after the first unbalanced right paren.
pub struct FuncTokens<'s, T> where T: Iterator<Item=Token<'s>> {
    tokens: T,
    parens: u32,
    unexpected_end: bool,
}

impl<'s, T> FuncTokens<'s, T> where T: Iterator<Item=Token<'s>> {
    /// Create a new iterator operating over an existing one.
    pub fn new(tokens: T) -> FuncTokens<'s, T> {
        FuncTokens {
            tokens,
            parens: 0,
            unexpected_end: false,
        }
    }
}

impl<'s, T> Iterator for FuncTokens<'s, T> where T: Iterator<Item=Token<'s>> {
    type Item = Token<'s>;

    fn next(&mut self) -> Option<Token<'s>> {
        let token = self.tokens.next();
        match token {
            Some(Token::RightBracket) if self.parens == 0 => None,
            Some(Token::RightBracket) => {
                self.parens -= 1;
                token
            },
            Some(Token::LeftBracket) => {
                self.parens += 1;
                token
            }
            None => {
                self.unexpected_end = true;
                None
            }
            token => token,
        }
    }
}

/// The error type for parsing.
pub struct ParseError {
    message: String,
}

impl ParseError {
    fn new<S: Into<String>>(message: S) -> ParseError {
        ParseError { message: message.into() }
    }
}

/// The result type for parsing.
pub type ParseResult<T> = Result<T, ParseError>;

error_type! {
    err: ParseError,
    show: f => f.write_str(&err.message),
}


#[cfg(test)]
mod token_tests {
    use super::*;
    use Token::{Space as S, Newline as N, LeftBracket as L, RightBracket as R,
                Colon as C, Equals as E, DoubleUnderscore as DU, DoubleStar as DS,
                Dollar as D, Hashtag as H, Word as W};

    /// Test if the source code tokenizes to the tokens.
    fn test(src: &str, tokens: Vec<Token>) {
        assert_eq!(Tokens::new(src).collect::<Vec<_>>(), tokens);
    }

    /// Tokenizes the basic building blocks.
    #[test]
    fn tokenize_base() {
        test("", vec![]);
        test("Hallo", vec![W("Hallo")]);
        test("[", vec![L]);
        test("]", vec![R]);
        test("$", vec![D]);
        test("#", vec![H]);
        test("**", vec![DS]);
        test("__", vec![DU]);
        test("\n", vec![N]);
    }

    /// This test looks if LF- and CRLF-style newlines get both identified correctly
    #[test]
    fn tokenize_whitespace_newlines() {
        test(" \t", vec![S]);
        test("First line\r\nSecond line\nThird line\n",
             vec![W("First"), S, W("line"), N, W("Second"), S, W("line"), N,
                  W("Third"), S, W("line"), N]);
        test("Hello \n ", vec![W("Hello"), S, N, S]);
        test("Dense\nTimes", vec![W("Dense"), N, W("Times")]);
    }

    /// Tests if escaping with backslash works as it should.
    #[test]
    fn tokenize_escape() {
        test(r"\[", vec![W("[")]);
        test(r"\]", vec![W("]")]);
        test(r"\#", vec![W("#")]);
        test(r"\$", vec![W("$")]);
        test(r"\:", vec![W(":")]);
        test(r"\=", vec![W("=")]);
        test(r"\**", vec![W("*"), W("*")]);
        test(r"\*", vec![W("*")]);
        test(r"\__", vec![W("__")]);
        test(r"\_", vec![W("_")]);
        test(r"\hello", vec![W(r"\"), W("hello")]);
    }

    /// Tokenizes some more realistic examples.
    #[test]
    fn tokenize_examples() {
        test(r"
            [function][
                Test [italic][example]!
            ]
        ", vec![
            N, S, L, W("function"), R, L, N, S, W("Test"), S, L, W("italic"), R, L,
            W("example"), R, W("!"), N, S, R, N, S
        ]);

        test(r"
            [page: size=A4]
            [font: size=12pt]

            Das ist ein Beispielsatz mit **fetter** Schrift.
        ", vec![
            N, S, L, W("page"), C, S, W("size"), E, W("A4"), R, N, S,
            L, W("font"), C, S, W("size"), E, W("12pt"), R, N, N, S,
            W("Das"), S, W("ist"), S, W("ein"), S, W("Beispielsatz"), S, W("mit"), S,
            DS, W("fetter"), DS, S, W("Schrift"), W("."), N, S
        ]);
    }

    /// This test checks whether the colon and equals symbols get parsed correctly
    /// depending on the context: Either in a function header or in a body.
    #[test]
    fn tokenize_symbols_context() {
        test("[func: key=value][Answer: 7]",
             vec![L, W("func"), C, S, W("key"), E, W("value"), R, L,
                  W("Answer"), W(":"), S, W("7"), R]);
        test("[[n: k=v]:x][:[=]]:=",
             vec![L, L, W("n"), C, S, W("k"), E, W("v"), R, C, W("x"), R,
                  L, W(":"), L, E, R, R, W(":"), W("=")]);
        test("[func: __key__=value]",
             vec![L, W("func"), C, S, DU, W("key"), DU, E, W("value"), R]);
    }

    /// This test has a special look at the double underscore syntax, because
    /// per Unicode standard they are not separate words and thus harder to parse
    /// than the stars.
    #[test]
    fn tokenize_double_underscore() {
        test("he__llo__world_ _ __ Now this_ is__ special!",
             vec![W("he"), DU, W("llo"), DU, W("world_"), S, W("_"), S, DU, S, W("Now"), S,
                  W("this_"), S, W("is"), DU, S, W("special"), W("!")]);
    }

    /// This test is for checking if non-ASCII characters get parsed correctly.
    #[test]
    fn tokenize_unicode() {
        test("[document][Hello 🌍!]",
             vec![L, W("document"), R, L, W("Hello"), S, W("🌍"), W("!"), R]);
        test("[f]⺐.", vec![L, W("f"), R, W("⺐"), W(".")]);
    }
}


#[cfg(test)]
mod parse_tests {
    use super::*;
    use crate::func::{Function, Scope, BodyTokens};
    use Node::{Space as S, Newline as N, Func as F};

    #[allow(non_snake_case)]
    fn W(s: &str) -> Node { Node::Word(s.to_owned()) }

    /// A testing function which just parses it's body into a syntax tree.
    #[derive(Debug, PartialEq)]
    struct TreeFn(SyntaxTree);

    impl Function for TreeFn {
        fn parse(_: &FuncHeader, tokens: BodyTokens<'_>, scope: &Scope)
        -> ParseResult<Self> where Self: Sized {
            if let Some(tokens) = tokens {
                Parser::with_scope(scope, tokens).parse().map(|tree| TreeFn(tree))
            } else {
                Err(ParseError::new("expected body for tree fn"))
            }
        }
        fn typeset(&self, _header: &FuncHeader) -> Option<Expression> { None }
    }

    /// A testing function without a body.
    #[derive(Debug, PartialEq)]
    struct BodylessFn;

    impl Function for BodylessFn {
        fn parse(_: &FuncHeader, tokens: BodyTokens<'_>, _: &Scope)
        -> ParseResult<Self> where Self: Sized {
            if tokens.is_none() {
                Ok(BodylessFn)
            } else {
                Err(ParseError::new("unexpected body for bodyless fn"))
            }
        }
        fn typeset(&self, _header: &FuncHeader) -> Option<Expression> { None }
    }

    /// Shortcut macro to create a function.
    macro_rules! func {
        (name => $name:expr, body => None $(,)*) => {
            func!(@$name, Box::new(BodylessFn))
        };
        (name => $name:expr, body => $tree:expr $(,)*) => {
            func!(@$name, Box::new(TreeFn($tree)))
        };
        (@$name:expr, $body:expr) => {
            FuncCall {
                header: FuncHeader {
                    name: Ident::new($name).unwrap(),
                    args: vec![],
                    kwargs: HashMap::new(),
                },
                body: $body,
            }
        }
    }

    /// Shortcut macro to create a syntax tree.
    /// Is `vec`-like and the elements are the nodes.
    macro_rules! tree {
        ($($x:expr),*) => (
            SyntaxTree { nodes: vec![$($x),*] }
        );
        ($($x:expr,)*) => (tree![$($x),*])
    }

    /// Test if the source code parses into the syntax tree.
    fn test(src: &str, tree: SyntaxTree) {
        assert_eq!(Parser::new(Tokens::new(src)).parse().unwrap(), tree);
    }

    /// Test with a scope containing function definitions.
    fn test_scoped(scope: &Scope, src: &str, tree: SyntaxTree) {
        assert_eq!(Parser::with_scope(scope, Tokens::new(src)).parse().unwrap(), tree);
    }

    /// Test if the source parses into the error.
    fn test_err(src: &str, err: &str) {
        assert_eq!(Parser::new(Tokens::new(src)).parse().unwrap_err().message, err);
    }

    /// Test with a scope if the source parses into the error.
    fn test_err_scoped(scope: &Scope, src: &str, err: &str) {
        assert_eq!(Parser::with_scope(scope, Tokens::new(src)).parse().unwrap_err().message, err);
    }

    /// Parse the basic cases.
    #[test]
    fn parse_base() {
        test("", tree! []);
        test("Hello World!", tree! [ W("Hello"), S, W("World"), W("!") ]);
    }

    /// Test whether newlines generate the correct whitespace.
    #[test]
    fn parse_newlines_whitespace() {
        test("Hello\nWorld", tree! [ W("Hello"), S, W("World") ]);
        test("Hello \n World", tree! [ W("Hello"), S, W("World") ]);
        test("Hello\n\nWorld", tree! [ W("Hello"), N, W("World") ]);
        test("Hello \n\nWorld", tree! [ W("Hello"), S, N, W("World") ]);
        test("Hello\n\n  World", tree! [ W("Hello"), N, S, W("World") ]);
        test("Hello \n \n \n  World", tree! [ W("Hello"), S, N, S, W("World") ]);
        test("Hello\n \n\n  World", tree! [ W("Hello"), S, N, S, W("World") ]);
    }

    /// Parse things dealing with functions.
    #[test]
    fn parse_functions() {
        let mut scope = Scope::new();
        scope.add::<BodylessFn>("test");
        scope.add::<BodylessFn>("end");
        scope.add::<TreeFn>("modifier");
        scope.add::<TreeFn>("func");

        test_scoped(&scope,"[test]", tree! [ F(func! { name => "test", body => None }) ]);
        test_scoped(&scope, "This is an [modifier][example] of a function invocation.", tree! [
            W("This"), S, W("is"), S, W("an"), S,
            F(func! { name => "modifier", body => tree! [ W("example") ] }), S,
            W("of"), S, W("a"), S, W("function"), S, W("invocation"), W(".")
        ]);
        test_scoped(&scope, "[func][Hello][modifier][Here][end]",  tree! [
            F(func! {
                name => "func",
                body => tree! [ W("Hello") ],
            }),
            F(func! {
                name => "modifier",
                body => tree! [ W("Here") ],
            }),
            F(func! {
                name => "end",
                body => None,
            }),
        ]);
        test_scoped(&scope, "[func][]", tree! [
            F(func! {
                name => "func",
                body => tree! [],
            })
        ]);
        test_scoped(&scope, "[modifier][[func][call]] outside", tree! [
            F(func! {
                name => "modifier",
                body => tree! [
                    F(func! {
                        name => "func",
                        body => tree! [ W("call") ],
                    }),
                ],
            }),
            S, W("outside")
        ]);
    }

    /// Tests if the parser handles non-ASCII stuff correctly.
    #[test]
    fn parse_unicode() {
        let mut scope = Scope::new();
        scope.add::<BodylessFn>("func");
        scope.add::<TreeFn>("bold");

        test_scoped(&scope, "[func] ⺐.", tree! [
            F(func! {
                name => "func",
                body => None,
            }),
            S, W("⺐"), W(".")
        ]);
        test_scoped(&scope, "[bold][Hello 🌍!]", tree! [
            F(func! {
                name => "bold",
                body => tree! [ W("Hello"), S, W("🌍"), W("!") ],
            })
        ]);
    }

    /// Tests whether errors get reported correctly.
    #[test]
    fn parse_errors() {
        let mut scope = Scope::new();
        scope.add::<TreeFn>("hello");

        test_err("No functions here]", "unexpected closing bracket");
        test_err_scoped(&scope, "[hello][world", "expected closing bracket");
        test_err("[hello world", "expected closing bracket");
        test_err("[ no-name][Why?]", "expected identifier");
    }
}
