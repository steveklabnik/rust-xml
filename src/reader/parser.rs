//! Contains an implementation of pull-based XML parser.

use std::mem;

use common;
use common::{Error, XmlVersion, Name, is_name_start_char, is_name_char, is_whitespace_char};
use namespace;
use namespace::{NamespaceStack};

use reader::events;
use reader::events::XmlEvent;
use reader::config::ParserConfig;
use reader::lexer;
use reader::lexer::{
    Token,
    PullLexer,
    ProcessingInstructionStart,
    ProcessingInstructionEnd,
    DoctypeStart,
    OpeningTagStart,
    ClosingTagStart,
    TagEnd,
    EmptyTagEnd,
    CommentStart,
    CommentEnd,
    Chunk,
    Character,
    Whitespace,
    CDataStart,
    CDataEnd,
    ReferenceStart,
    ReferenceEnd,
    DoubleQuote,
    SingleQuote,
    EqualsSign
};

static DEFAULT_VERSION: XmlVersion      = common::Version10;
static DEFAULT_ENCODING: &'static str   = "UTF-8";
static DEFAULT_STANDALONE: Option<bool> = None;

type ElementStack = Vec<Name>;

/// Pull-based XML parser.
pub struct PullParser {
    config: ParserConfig,
    lexer: PullLexer,
    st: State,
    buf: String,
    nst: NamespaceStack,

    data: MarkupData,
    finish_event: Option<XmlEvent>,
    next_event: Option<XmlEvent>,
    est: ElementStack,

    encountered_element: bool,
    parsed_declaration: bool,
    inside_whitespace: bool,
    read_prefix_separator: bool,
    pop_namespace: bool
}

impl PullParser {
    /// Returns a new parser using the given config.
    pub fn new(config: ParserConfig) -> PullParser {
        PullParser {
            config: config,
            lexer: lexer::new(),
            st: OutsideTag,
            buf: String::new(),
            nst: NamespaceStack::default(),

            data: MarkupData {
                name: String::new(),
                version: None,
                encoding: None,
                standalone: None,
                ref_data: String::new(),
                element_name: None,
                quote: None,
                attr_name: None,
                attributes: vec!()
            },
            finish_event: None,
            next_event: None,
            est: Vec::new(),

            encountered_element: false,
            parsed_declaration: false,
            inside_whitespace: true,
            read_prefix_separator: false,
            pop_namespace: false
        }
    }
}

#[deriving(Clone, PartialEq)]
enum State {
    OutsideTag,
    InsideOpeningTag(OpeningTagSubstate),
    InsideClosingTag(ClosingTagSubstate),
    InsideProcessingInstruction(ProcessingInstructionSubstate),
    InsideComment,
    InsideCData,
    InsideDeclaration(DeclarationSubstate),
    InsideDoctype,
    InsideReference(Box<State>)
}

#[deriving(Clone, PartialEq)]
enum OpeningTagSubstate {
    InsideName,

    InsideTag,

    InsideAttributeName,
    AfterAttributeName,

    InsideAttributeValue,
}

#[deriving(Clone, PartialEq)]
enum ClosingTagSubstate {
    CTInsideName,
    CTAfterName
}

#[deriving(Clone, PartialEq)]
enum ProcessingInstructionSubstate {
    PIInsideName,
    PIInsideData
}

#[deriving(Clone, PartialEq)]
enum DeclarationSubstate {
    BeforeVersion,
    InsideVersion,
    AfterVersion,

    InsideVersionValue,
    AfterVersionValue,

    InsideEncoding,
    AfterEncoding,

    InsideEncodingValue,

    BeforeStandaloneDecl,
    InsideStandaloneDecl,
    AfterStandaloneDecl,

    InsideStandaloneDeclValue,
    AfterStandaloneDeclValue
}

#[deriving(PartialEq)]
enum QualifiedNameTarget {
    AttributeNameTarget,
    OpeningTagNameTarget,
    ClosingTagNameTarget
}

#[deriving(PartialEq, Eq)]
enum QuoteToken {
    SingleQuoteToken,
    DoubleQuoteToken
}

impl QuoteToken {
    fn from_token(t: &Token) -> QuoteToken {
        match *t {
            SingleQuote => SingleQuoteToken,
            DoubleQuote => DoubleQuoteToken,
            _ => panic!("Unexpected token: {}", t)
        }
    }

    fn as_token(self) -> Token {
        match self {
            SingleQuoteToken => SingleQuote,
            DoubleQuoteToken => DoubleQuote
        }
    }
}

struct AttributeData {
    name: Name,
    value: String
}

impl AttributeData {
    fn into_attribute(self) -> common::Attribute {
        let AttributeData { name, value } = self;
        common::Attribute {
            name: name,
            value: value
        }
    }
}

struct MarkupData {
    name: String,     // used for processing instruction name
    ref_data: String,  // used for reference content

    version: Option<common::XmlVersion>,  // used for XML declaration version
    encoding: Option<String>,  // used for XML declaration encoding
    standalone: Option<bool>,  // used for XML declaration standalone parameter

    element_name: Option<Name>,  // used for element name

    quote: Option<QuoteToken>,  // used to hold opening quote for attribute value
    attr_name: Option<Name>,  // used to hold attribute name
    attributes: Vec<AttributeData>   // used to hold all accumulated attributes
}

macro_rules! gen_takes(
    ($($field:ident -> $method:ident, $t:ty, $def:expr);+) => (
        $(
        impl MarkupData {
            #[inline]
            fn $method(&mut self) -> $t {
                mem::replace(&mut self.$field, $def)
            }
        }
        )+
    )
)

gen_takes!(
    name         -> take_name, String, String::new();
    ref_data     -> take_ref_data, String, String::new();

    version      -> take_version, Option<common::XmlVersion>, None;
    encoding     -> take_encoding, Option<String>, None;
    standalone   -> take_standalone, Option<bool>, None;

    element_name -> take_element_name, Option<Name>, None;

    attr_name    -> take_attr_name, Option<Name>, None;
    attributes   -> take_attributes, Vec<AttributeData>, vec!()
)

macro_rules! self_error(
    ($this:ident; $msg:expr) => ($this.error($msg.to_string()));
    ($this:ident; $fmt:expr, $($arg:expr),+) => ($this.error(format!($fmt, $($arg),+)))
)

impl PullParser {
    /// Returns next event read from the given buffer.
    ///
    /// This method should be always called with the same buffer. If you call it
    /// providing different buffers each time, the result will be undefined.
    pub fn next<B: Buffer>(&mut self, r: &mut B) -> XmlEvent {
        if self.finish_event.is_some() {
            return self.finish_event.as_ref().unwrap().clone();
        }

        if self.next_event.is_some() {
            return mem::replace(&mut self.next_event, None).unwrap();
        }

        if self.pop_namespace {
            self.pop_namespace = false;
            self.nst.pop();
        }

        for_each!(t in self.lexer.next_token(r) {
            match t {
                Ok(t) => match self.dispatch_token(t) {
                    Some(ev) => {
                        match ev {
                            events::EndDocument | events::Error(_) =>
                                self.finish_event = Some(ev.clone()),
                            _ => {}
                        }
                        return ev;
                    }
                    None => {}  // continue
                },

                // Pass through unexpected lexer errors
                Err(e) => {
                    let ev = events::Error(e);
                    self.finish_event = Some(ev.clone());
                    return ev;
                }
            }
        })

        // Handle end of stream
        let ev = if self.depth() == 0 {
            if self.encountered_element && self.st == OutsideTag {  // all is ok
                events::EndDocument
            } else if !self.encountered_element {
                self_error!(self; "Unexpected end of stream: no root element found")
            } else {  // self.st != OutsideTag
                self_error!(self; "Unexpected end of stream")  // TODO: add expected hint?
            }
        } else {
            self_error!(self; "Unexpected end of stream: still inside the root element")
        };
        self.finish_event = Some(ev.clone());
        ev
    }

    #[inline]
    fn error(&self, msg: String) -> XmlEvent {
        events::Error(Error::new(&self.lexer, msg))
    }

    fn dispatch_token(&mut self, t: Token) -> Option<XmlEvent> {
        match self.st.clone() {
            OutsideTag                     => self.outside_tag(t),
            InsideProcessingInstruction(s) => self.inside_processing_instruction(t, s),
            InsideDeclaration(s)           => self.inside_declaration(t, s),
            InsideDoctype                  => self.inside_doctype(t),
            InsideOpeningTag(s)            => self.inside_opening_tag(t, s),
            InsideClosingTag(s)            => self.inside_closing_tag_name(t, s),
            InsideComment                  => self.inside_comment(t),
            InsideCData                    => self.inside_cdata(t),
            InsideReference(s)             => self.inside_reference(t, *s)
        }
    }

    #[inline]
    fn depth(&self) -> uint {
        self.est.len()
    }

    #[inline]
    fn buf_has_data(&self) -> bool {
        self.buf.len() > 0
    }

    #[inline]
    fn take_buf(&mut self) -> String {
        mem::replace(&mut self.buf, String::new())
    }

    #[inline]
    fn append_char_continue(&mut self, c: char) -> Option<XmlEvent> {
        self.buf.push(c);
        None
    }

    #[inline]
    fn append_str_continue(&mut self, s: &str) -> Option<XmlEvent> {
        self.buf.push_str(s);
        None
    }

    #[inline]
    fn into_state(&mut self, st: State, ev: Option<XmlEvent>) -> Option<XmlEvent> {
        self.st = st;
        ev
    }

    #[inline]
    fn into_state_continue(&mut self, st: State) -> Option<XmlEvent> {
        self.into_state(st, None)
    }

    #[inline]
    fn into_state_emit(&mut self, st: State, ev: XmlEvent) -> Option<XmlEvent> {
        self.into_state(st, Some(ev))
    }

    /// Dispatches tokens in order to process qualified name. If qualified name cannot be parsed,
    /// an error is returned.
    ///
    /// # Parameters
    /// * `t`       --- next token;
    /// * `on_name` --- a callback which is executed when whitespace is encountered.
    fn read_qualified_name(&mut self, t: Token, target: QualifiedNameTarget,
                           on_name: |&mut PullParser, Token, Name| -> Option<XmlEvent>) -> Option<XmlEvent> {
        // We can get here for the first time only when self.data.name contains zero or one character,
        // but first character cannot be a colon anyway
        if self.buf.len() <= 1 {
            self.read_prefix_separator = false;
        }

        let invoke_callback = |this: &mut PullParser, t| {
            let name = this.take_buf();
            match common::parse_name(name.as_slice()) {
                Some(name) => on_name(this, t, name),
                None => Some(self_error!(this; "Qualified name is invalid: {}", name))
            }
        };

        match t {
            // There can be only one colon, and not as the first character
            Character(':') if self.buf_has_data() && !self.read_prefix_separator => {
                self.buf.push(':');
                self.read_prefix_separator = true;
                None
            }

            Character(c) if c != ':' && (!self.buf_has_data() && is_name_start_char(c) ||
                                          self.buf_has_data() && is_name_char(c)) =>
                self.append_char_continue(c),

            EqualsSign if target == AttributeNameTarget => invoke_callback(self, t),

            EmptyTagEnd if target == OpeningTagNameTarget => invoke_callback(self, t),

            TagEnd if target == OpeningTagNameTarget ||
                      target == ClosingTagNameTarget => invoke_callback(self, t),

            Whitespace(_) => invoke_callback(self, t),

            _ => Some(self_error!(self; "Unexpected token inside qualified name: {}", t))
        }
    }

    /// Dispatches tokens in order to process attribute value.
    ///
    /// # Parameters
    /// * `t`        --- next token;
    /// * `on_value` --- a callback which is called when terminating quote is encountered.
    fn read_attribute_value(&mut self, t: Token, on_value: |&mut PullParser, String| -> Option<XmlEvent>) -> Option<XmlEvent> {
        match t {
            Whitespace(_) if self.data.quote.is_none() => None,  // skip leading whitespace

            DoubleQuote | SingleQuote => match self.data.quote {
                None => {  // Entered attribute value
                    self.data.quote = Some(QuoteToken::from_token(&t));
                    None
                }
                Some(q) if q.as_token() == t => {
                    self.data.quote = None;
                    let value = self.take_buf();
                    on_value(self, value)
                }
                _ => self.append_str_continue(t.to_string().as_slice()),
            },

            ReferenceStart => {
                let st = box self.st.clone();
                self.into_state_continue(InsideReference(st))
            }

            OpeningTagStart =>
                Some(self_error!(self; "Unexpected token inside attribute value: <")),

            // Every character except " and ' and < is okay
            _  => self.append_str_continue(t.to_string().as_slice()),
        }
    }

    fn outside_tag(&mut self, t: Token) -> Option<XmlEvent> {
        match t {
            ReferenceStart =>
                self.into_state_continue(InsideReference(box OutsideTag)),

            Whitespace(_) if self.depth() == 0 => None,  // skip whitespace outside of the root element

            _ if t.contains_char_data() && self.depth() == 0 =>
                Some(self_error!(self; "Unexpected characters outside the root element: {}", t)),

            Whitespace(c) => self.append_char_continue(c),

            _ if t.contains_char_data() => {  // Non-whitespace char data
                self.inside_whitespace = false;
                self.append_str_continue(t.to_string().as_slice())
            }

            ReferenceEnd => { // Semi-colon in a text outside an entity
                self.inside_whitespace = false;
                self.append_str_continue(ReferenceEnd.as_static_str().unwrap())
            }

            CommentStart if self.config.coalesce_characters && self.config.ignore_comments => {
                // We need to disable lexing errors inside comments
                self.lexer.disable_errors();
                self.into_state_continue(InsideComment)
            }

            CDataStart if self.config.coalesce_characters && self.config.cdata_to_characters => {
                // We need to disable lexing errors inside CDATA
                self.lexer.disable_errors();
                self.into_state_continue(InsideCData)
            }

            _ => {
                // Encountered some markup event, flush the buffer as characters
                // or a whitespace
                let mut next_event = if self.buf_has_data() {
                    let buf = self.take_buf();
                    if self.inside_whitespace && self.config.trim_whitespace {
                        None
                    } else if self.inside_whitespace && !self.config.whitespace_to_characters {
                        Some(events::Whitespace(buf))
                    } else if self.config.trim_whitespace {
                        Some(events::Characters(buf.as_slice().trim_chars(is_whitespace_char).to_string()))
                    } else {
                        Some(events::Characters(buf))
                    }
                } else { None };
                self.inside_whitespace = true;  // Reset inside_whitespace flag
                match t {
                    ProcessingInstructionStart =>
                        self.into_state(InsideProcessingInstruction(PIInsideName), next_event),

                    DoctypeStart if !self.encountered_element => {
                        self.lexer.disable_errors();
                        self.into_state(InsideDoctype, next_event)
                    }

                    OpeningTagStart => {
                        // If declaration was not parsed and we have encountered an element,
                        // emit this declaration as the next event.
                        if !self.parsed_declaration {
                            self.parsed_declaration = true;
                            let sd_event = events::StartDocument {
                                version: DEFAULT_VERSION,
                                encoding: DEFAULT_ENCODING.to_string(),
                                standalone: DEFAULT_STANDALONE
                            };
                            // next_event is always none here because we're outside of
                            // the root element
                            next_event = Some(sd_event);
                        }
                        self.encountered_element = true;
                        self.nst.push_empty();
                        self.into_state(InsideOpeningTag(InsideName), next_event)
                    }

                    ClosingTagStart if self.depth() > 0 =>
                        self.into_state(InsideClosingTag(CTInsideName), next_event),

                    CommentStart => {
                        // We need to disable lexing errors inside comments
                        self.lexer.disable_errors();
                        self.into_state(InsideComment, next_event)
                    }

                    CDataStart => {
                        // We need to disable lexing errors inside CDATA
                        self.lexer.disable_errors();
                        self.into_state(InsideCData, next_event)
                    }

                    _ => Some(self_error!(self; "Unexpected token: {}", t))
                }
            }
        }
    }

    fn inside_doctype(&mut self, t: Token) -> Option<XmlEvent> {
        match t {
            TagEnd => {
                self.lexer.enable_errors();
                self.into_state_continue(OutsideTag)
            }

            _ => None
        }
    }

    fn inside_processing_instruction(&mut self, t: Token, s: ProcessingInstructionSubstate) -> Option<XmlEvent> {
        match s {
            PIInsideName => match t {
                Character(c) if !self.buf_has_data() && is_name_start_char(c) ||
                                 self.buf_has_data() && is_name_char(c) => self.append_char_continue(c),

                ProcessingInstructionEnd => {
                    // self.buf contains PI name
                    let name = self.take_buf();

                    // Don't need to check for declaration because it has mandatory attributes
                    // but there is none
                    match name.as_slice() {
                        // Name is empty, it is an error
                        "" => Some(self_error!(self; "Encountered processing instruction without name")),

                        // Found <?xml-like PI not at the beginning of a document,
                        // it is an error - see section 2.6 of XML 1.1 spec
                        "xml"|"xmL"|"xMl"|"xML"|"Xml"|"XmL"|"XMl"|"XML" =>
                            Some(self_error!(self; "Invalid processing instruction: <?{}", name)),

                        // All is ok, emitting event
                        _ => {
                            self.into_state_emit(
                                OutsideTag,
                                events::ProcessingInstruction {
                                    name: name,
                                    data: None
                                }
                            )
                        }
                    }
                }

                Whitespace(_) => {
                    // self.buf contains PI name
                    let name = self.take_buf();

                    match name.as_slice() {
                        // We have not ever encountered an element and have not parsed XML declaration
                        "xml" if !self.encountered_element && !self.parsed_declaration =>
                            self.into_state_continue(InsideDeclaration(BeforeVersion)),

                        // Found <?xml-like PI after the beginning of a document,
                        // it is an error - see section 2.6 of XML 1.1 spec
                        "xml"|"xmL"|"xMl"|"xML"|"Xml"|"XmL"|"XMl"|"XML"
                            if self.encountered_element || self.parsed_declaration =>
                            Some(self_error!(self; "Invalid processing instruction: <?{}", name)),

                        // All is ok, starting parsing PI data
                        _ => {
                            self.lexer.disable_errors();  // data is arbitrary, so disable errors
                            self.data.name = name;
                            self.into_state_continue(InsideProcessingInstruction(PIInsideData))
                        }

                    }
                }

                _ => Some(self_error!(self; "Unexpected token: <?{}{}", self.buf, t))
            },

            PIInsideData => match t {
                ProcessingInstructionEnd => {
                    self.lexer.enable_errors();
                    let name = self.data.take_name();
                    let data = self.take_buf();
                    self.into_state_emit(
                        OutsideTag,
                        events::ProcessingInstruction {
                            name: name,
                            data: Some(data)
                        }
                    )
                },

                // Any other token should be treated as plain characters
                _ => {
                    self.buf.push_str(t.to_string().as_slice());
                    None
                }
            },
        }
    }

    // TODO: remove redundancy via macros or extra methods
    fn inside_declaration(&mut self, t: Token, s: DeclarationSubstate) -> Option<XmlEvent> {
        macro_rules! unexpected_token(
            ($this:expr; $t:expr) => (Some($this.error(format!("Unexpected token inside XML declaration: {}", $t))));
            ($t:expr) => (unexpected_token!(self; $t));
        )

        #[inline]
        fn emit_start_document(this: &mut PullParser) -> Option<XmlEvent> {
            this.parsed_declaration = true;
            let version = this.data.take_version();
            let encoding = this.data.take_encoding();
            let standalone = this.data.take_standalone();
            this.into_state_emit(OutsideTag, events::StartDocument {
                version: version.unwrap_or(DEFAULT_VERSION),
                encoding: encoding.unwrap_or(DEFAULT_ENCODING.to_string()),
                standalone: standalone
            })
        }

        match s {
            BeforeVersion => match t {
                Whitespace(_) => None,  // continue
                Character('v') => self.into_state_continue(InsideDeclaration(InsideVersion)),
                _ => unexpected_token!(t)
            },

            InsideVersion => self.read_qualified_name(t, AttributeNameTarget, |this, token, name| {
                match name.local_name.as_slice() {
                    "ersion" if name.namespace.is_none() =>
                        this.into_state_continue(InsideDeclaration(
                            if token == EqualsSign { InsideVersionValue } else { AfterVersion }
                        )),
                    _ => unexpected_token!(this; name)
                }
            }),

            AfterVersion => match t {
                Whitespace(_) => None,
                EqualsSign => self.into_state_continue(InsideDeclaration(InsideVersionValue)),
                _ => unexpected_token!(t)
            },

            InsideVersionValue => self.read_attribute_value(t, |this, value| {
                this.data.version = match value.as_slice() {
                    "1.0" => Some(common::Version10),
                    "1.1" => Some(common::Version11),
                    _     => None
                };
                if this.data.version.is_some() {
                    this.into_state_continue(InsideDeclaration(AfterVersionValue))
                } else {
                    Some(self_error!(this; "Unexpected XML version value: {}", value))
                }
            }),

            AfterVersionValue => match t {
                Whitespace(_) => None,  // skip whitespace
                Character('e') => self.into_state_continue(InsideDeclaration(InsideEncoding)),
                Character('s') => self.into_state_continue(InsideDeclaration(InsideStandaloneDecl)),
                ProcessingInstructionEnd => emit_start_document(self),
                _ => unexpected_token!(t)
            },

            InsideEncoding => self.read_qualified_name(t, AttributeNameTarget, |this, token, name| {
                match name.local_name.as_slice() {
                    "ncoding" if name.namespace.is_none() =>
                        this.into_state_continue(InsideDeclaration(
                            if token == EqualsSign { InsideEncodingValue } else { AfterEncoding }
                        )),
                    _ => unexpected_token!(this; name)
                }
            }),

            AfterEncoding => match t {
                Whitespace(_) => None,
                EqualsSign => self.into_state_continue(InsideDeclaration(InsideEncodingValue)),
                _ => unexpected_token!(t)
            },

            InsideEncodingValue => self.read_attribute_value(t, |this, value| {
                this.data.encoding = Some(value);
                this.into_state_continue(InsideDeclaration(BeforeStandaloneDecl))
            }),

            BeforeStandaloneDecl => match t {
                Whitespace(_) => None,  // skip whitespace
                Character('s') => self.into_state_continue(InsideDeclaration(InsideStandaloneDecl)),
                ProcessingInstructionEnd => emit_start_document(self),
                _ => unexpected_token!(t)
            },

            InsideStandaloneDecl => self.read_qualified_name(t, AttributeNameTarget, |this, token, name| {
                match name.local_name.as_slice() {
                    "tandalone" if name.namespace.is_none() =>
                        this.into_state_continue(InsideDeclaration(
                            if token == EqualsSign { InsideStandaloneDeclValue } else { AfterStandaloneDecl }
                        )),
                    _ => unexpected_token!(this; name)
                }
            }),

            AfterStandaloneDecl => match t {
                Whitespace(_) => None,
                EqualsSign => self.into_state_continue(InsideDeclaration(InsideStandaloneDeclValue)),
                _ => unexpected_token!(t)
            },

            InsideStandaloneDeclValue => self.read_attribute_value(t, |this, value| {
                let standalone = match value.as_slice() {
                    "yes" => Some(true),
                    "no"  => Some(false),
                    _     => None
                };
                if standalone.is_some() {
                    this.data.standalone = standalone;
                    this.into_state_continue(InsideDeclaration(AfterStandaloneDeclValue))
                } else {
                    Some(self_error!(this; "Invalid standalone declaration value: {}", value))
                }
            }),

            AfterStandaloneDeclValue => match t {
                Whitespace(_) => None,  // skip whitespace
                ProcessingInstructionEnd => emit_start_document(self),
                _ => unexpected_token!(t)
            }
        }
    }

    #[inline]
    fn emit_start_element(&mut self, emit_end_element: bool) -> Option<XmlEvent> {
        let mut name = self.data.take_element_name().unwrap();
        let mut attributes = self.data.take_attributes();

        // check whether the name prefix is bound and fix its namespace
        match self.nst.get(&name.prefix) {
            Some("") => name.namespace = None,  // default namespace
            Some(ns) => name.namespace = Some(ns.to_string()),
            None => return Some(self_error!(self; "Element {} prefix is unbound", name))
        }

        // check and fix accumulated attributes prefixes
        for attr in attributes.iter_mut() {
            match self.nst.get(&attr.name.prefix) {
                Some("") => attr.name.namespace = None,  // default namespace
                Some(ns) => attr.name.namespace = Some(ns.to_string()),
                None => return Some(self_error!(self; "Attribute {} prefix is unbound", attr.name))
            }
        }

        if emit_end_element {
            self.pop_namespace = true;
            self.next_event = Some(events::EndElement {
                name: name.clone()
            });
        } else {
            self.est.push(name.clone());
        }
        let namespace = self.nst.squash();
        self.into_state_emit(OutsideTag, events::StartElement {
            name: name,
            attributes: attributes.into_iter().map(|a| a.into_attribute()).collect(),
            namespace: namespace
        })
    }

    fn inside_opening_tag(&mut self, t: Token, s: OpeningTagSubstate) -> Option<XmlEvent> {
        macro_rules! unexpected_token(($t:expr) => (Some(self_error!(self; "Unexpected token inside opening tag: {}", $t))))
        match s {
            InsideName => self.read_qualified_name(t, OpeningTagNameTarget, |this, token, name| {
                match name.prefix_ref() {
                    Some(prefix) if prefix == namespace::NS_XML_PREFIX ||
                                    prefix == namespace::NS_XMLNS_PREFIX =>
                        Some(self_error!(this; "'{}' cannot be an element name prefix", name.prefix)),
                    _ => {
                        this.data.element_name = Some(name.clone());
                        match token {
                            TagEnd => this.emit_start_element(false),
                            EmptyTagEnd => this.emit_start_element(true),
                            Whitespace(_) => this.into_state_continue(InsideOpeningTag(InsideTag)),
                            _ => unreachable!()
                        }
                    }
                }
            }),

            InsideTag => match t {
                Whitespace(_) => None,  // skip whitespace
                Character(c) if is_name_start_char(c) => {
                    self.buf.push(c);
                    self.into_state_continue(InsideOpeningTag(InsideAttributeName))
                }
                TagEnd => self.emit_start_element(false),
                EmptyTagEnd => self.emit_start_element(true),
                _ => unexpected_token!(t.to_string())
            },

            InsideAttributeName => self.read_qualified_name(t, AttributeNameTarget, |this, token, name| {
                this.data.attr_name = Some(name);
                match token {
                    Whitespace(_) => this.into_state_continue(InsideOpeningTag(AfterAttributeName)),
                    EqualsSign => this.into_state_continue(InsideOpeningTag(InsideAttributeValue)),
                    _ => unreachable!()
                }
            }),

            AfterAttributeName => match t {
                Whitespace(_) => None,
                EqualsSign => self.into_state_continue(InsideOpeningTag(InsideAttributeValue)),
                _ => unexpected_token!(t.to_string())
            },

            InsideAttributeValue => self.read_attribute_value(t, |this, value| {
                let name = this.data.take_attr_name().unwrap();  // unwrap() will always succeed here
                match name.prefix_ref() {
                    // declaring a new prefix; it is sufficient to check prefix only
                    // because "xmlns" prefix is reserved
                    Some(prefix) if prefix == namespace::NS_XMLNS_PREFIX => {
                        let ln = name.local_name.as_slice();
                        if ln == namespace::NS_XMLNS_PREFIX {
                            Some(self_error!(this; "Cannot redefine '{}' prefix", namespace::NS_XMLNS_PREFIX))
                        } else if ln == namespace::NS_XML_PREFIX && value.as_slice() != namespace::NS_XML_URI {
                            Some(self_error!(this; "'{}' prefix cannot be rebound to another value", namespace::NS_XML_PREFIX))
                        } else if value.is_empty() {
                            Some(self_error!(this; "Cannot undefine a prefix: {}", ln))
                        } else {
                            this.nst.put(Some(name.local_name.clone()), value);
                            this.into_state_continue(InsideOpeningTag(InsideTag))
                        }
                    }

                    // declaring default namespace
                    None if name.local_name.as_slice() == namespace::NS_XMLNS_PREFIX =>
                        match value.as_slice() {
                            val if val == namespace::NS_XMLNS_PREFIX ||
                                   val == namespace::NS_XML_PREFIX =>
                                Some(self_error!(this; "Namespace '{}' cannot be default", value)),
                            _ => {
                                this.nst.put(None, value.clone());
                                this.into_state_continue(InsideOpeningTag(InsideTag))
                            }
                        },

                    // Plain attribute
                    _ => {
                        this.data.attributes.push(AttributeData {
                            name: name.clone(),
                            value: value
                        });
                        this.into_state_continue(InsideOpeningTag(InsideTag))
                    }
                }
            })
        }
    }

    #[inline]
    fn emit_end_element(&mut self) -> Option<XmlEvent> {
        let mut name = self.data.take_element_name().unwrap();

        // check whether the name prefix is bound and fix its namespace
        match self.nst.get(&name.prefix) {
            Some("") => name.namespace = None,  // default namespace
            Some(ns) => name.namespace = Some(ns.to_string()),
            None => return Some(self_error!(self; "Element {} prefix is unbound", name))
        }

        let op_name = self.est.pop().unwrap();

        if name == op_name {
            self.pop_namespace = true;
            self.into_state_emit(OutsideTag, events::EndElement { name: name })
        } else {
            Some(self_error!(self; "Unexpected closing tag: {}, expected {}", name, op_name))
        }
    }

    fn inside_closing_tag_name(&mut self, t: Token, s: ClosingTagSubstate) -> Option<XmlEvent> {
        match s {
            CTInsideName => self.read_qualified_name(t, ClosingTagNameTarget, |this, token, name| {
                match name.prefix_ref() {
                    Some(prefix) if prefix == namespace::NS_XML_PREFIX ||
                                    prefix == namespace::NS_XMLNS_PREFIX =>
                        Some(self_error!(this; "'{}' cannot be an element name prefix", name.prefix)),
                    _ => {
                        this.data.element_name = Some(name.clone());
                        match token {
                            Whitespace(_) => this.into_state_continue(InsideClosingTag(CTAfterName)),
                            TagEnd => this.emit_end_element(),
                            _ => Some(self_error!(this; "Unexpected token inside closing tag: {}", token))
                        }
                    }
                }
            }),
            CTAfterName => match t {
                Whitespace(_) => None,  //  Skip whitespace
                TagEnd => self.emit_end_element(),
                _ => Some(self_error!(self; "Unexpected token inside closing tag: {}", t))
            }
        }
    }

    fn inside_comment(&mut self, t: Token) -> Option<XmlEvent> {
        match t {
            // Double dash is illegal inside a comment
            Chunk(ref s) if s.as_slice() == "--" => Some(self_error!(self; "Unexpected token inside a comment: --")),

            CommentEnd if self.config.ignore_comments => {
                self.lexer.enable_errors();
                self.into_state_continue(OutsideTag)
            }

            CommentEnd => {
                self.lexer.enable_errors();
                let data = self.take_buf();
                self.into_state_emit(OutsideTag, events::Comment(data))
            }

            _ if self.config.ignore_comments => None,  // Do not modify buffer if ignoring the comment

            _ => self.append_str_continue(t.to_string().as_slice()),
        }
    }

    fn inside_cdata(&mut self, t: Token) -> Option<XmlEvent> {
        match t {
            CDataEnd => {
                self.lexer.enable_errors();
                let event = if self.config.cdata_to_characters {
                    None
                } else {
                    let data = self.take_buf();
                    Some(events::CData(data))
                };
                self.into_state(OutsideTag, event)
            }

            Whitespace(_) => self.append_str_continue(t.to_string().as_slice()),

            _ => {
                self.inside_whitespace = false;
                self.append_str_continue(t.to_string().as_slice())
            }
        }
    }

    fn inside_reference(&mut self, t: Token, prev_st: State) -> Option<XmlEvent> {
        use std::char;
        use std::num::from_str_radix;

        match t {
            Character(c) if !self.data.ref_data.is_empty() && is_name_char(c) ||
                             self.data.ref_data.is_empty() && (is_name_start_char(c) || c == '#') => {
                self.data.ref_data.push(c);
                None
            }

            ReferenceEnd => {
                // TODO: check for unicode correctness
                let name = self.data.take_ref_data();
                let name_len = name.len();  // compute once
                let c = match name.as_slice() {
                    "lt"   => Ok('<'),
                    "gt"   => Ok('>'),
                    "amp"  => Ok('&'),
                    "apos" => Ok('\''),
                    "quot" => Ok('"'),
                    ""     => Err(self_error!(self; "Encountered empty entity")),
                    _ if name_len > 2 && name.as_slice().slice(0, 2) == "#x" => {
                        let num_str = name.as_slice().slice(2, name_len);
                        if num_str == "0" {
                            Err(self_error!(self; "Null character entity is not allowed"))
                        } else {
                            match from_str_radix(num_str, 16).and_then(char::from_u32) {
                                Some(c) => Ok(c),
                                None    => Err(self_error!(self; "Invalid hexadecimal character number in an entity: {}", name))
                            }
                        }
                    }
                    _ if name_len > 1 && name.as_slice().char_at(0) == '#' => {
                        let num_str = name.as_slice().slice(1, name_len);
                        if num_str == "0" {
                            Err(self_error!(self; "Null character entity is not allowed"))
                        } else {
                            match from_str_radix(num_str, 10).and_then(char::from_u32) {
                                Some(c) => Ok(c),
                                None    => Err(self_error!(self; "Invalid decimal character number in an entity: {}", name))
                            }
                        }
                    },
                    _ => Err(self_error!(self; "Unexpected entity: {}", name))
                };
                match c {
                    Ok(c) => {
                        self.buf.push(c);
                        self.into_state_continue(prev_st)
                    }
                    Err(e) => Some(e)
                }
            }

            _ => Some(self_error!(self; "Unexpected token inside an entity: {}", t))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;

    use common::{Name, Attribute};
    use reader::parser::PullParser;
    use reader::ParserConfig;
    use reader::events;

    fn new_parser() -> PullParser {
        PullParser::new(ParserConfig::new())
    }

    macro_rules! expect_event(
        ($r:expr, $p:expr, $t:pat) => (
            match $p.next(&mut $r) {
                $t => {}
                e => panic!("Unexpected event: {}", e)
            }
        );
        ($r:expr, $p:expr, $t:pat if $c:expr) => (
            match $p.next(&mut $r) {
                $t if $c => {}
                e => panic!("Unexpected event: {}", e)
            }
        )
    )

    macro_rules! test_data(
        ($d:expr) => ({
            static DATA: &'static str = $d;
            let r = BufReader::new(DATA.as_bytes());
            let p = new_parser();
            (r, p)
        })
    )

    #[test]
    fn semicolon_in_attribute_value__issue_3() {
        let (mut r, mut p) = test_data!(r#"
            <a attr="zzz;zzz" />
        "#);

        expect_event!(r, p, events::StartDocument { .. });
        expect_event!(r, p, events::StartElement { ref name, ref attributes, ref namespace }
            if *name == Name::new_local("a") &&
               attributes.len() == 1 &&
               attributes[0] == Attribute::new_local("attr", "zzz;zzz") &&
               namespace.is_essentially_empty()
        );
        expect_event!(r, p, events::EndElement { ref name } if *name == Name::new_local("a"));
        expect_event!(r, p, events::EndDocument);
    }

    #[test]
    fn opening_tag_in_attribute_value() {
        let (mut r, mut p) = test_data!(r#"
            <a attr="zzz<zzz" />
        "#);

        expect_event!(r, p, events::StartDocument { .. });
        expect_event!(r, p, events::Error(ref e)
            if e.msg() == "Unexpected token inside attribute value: <"
        );
    }
}
