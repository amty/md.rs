use std::collections::HashMap;
use std::collections::VecDeque;
use std::cell::{RefCell, Cell};
use std::ops::Deref;

pub use self::config::*;
use tokens::*;

pub use self::ParseResult::*;
use self::block::BlockParser;

use util::{CellOps, ByteMatcher};

macro_rules! first_of {
    ($e:expr) => ($e);
    ($e:expr, $($more:expr),+) => (
        $e.or_else(|| first_of!($($more),*))
    )
}

macro_rules! one_of {
    ($c:expr, $p:expr) => ($c == $p);
    ($c:expr, $p:expr, $($rest:expr),+) => (
        ($c == $p || one_of!($c, $($rest),+))
    )
}

macro_rules! opt_ret_end {
    ($e:expr) => (
        match $e {
            None => return End,
            Some(r) => r
        }
    )
}

macro_rules! opt_break {
    ($e:expr) => (
        match $e {
            None => break,
            Some(r) => r
        }
    )
}

macro_rules! opt_ret {
    ($e:expr) => (
        match $e {
            None => return None,
            Some(r) => r
        }
    )
}

macro_rules! parse_or_ret {
    ($e:expr) => (
        match $e {
            NoParse => return NoParse,
            End => return End,
            Success(r) => r
        }
    )
}

macro_rules! parse_or_ret_none {
    ($e:expr) => (
        match $e {
            NoParse | End => return None,
            Success(r) => r
        }
    )
}

macro_rules! parse_or_break {
    ($e:expr) => (
        match $e {
            NoParse | End => break,
            o => o
        }
    )
}

macro_rules! break_on_end {
    ($e:expr) => (
        match $e {
            End => break,
            o => o
        }
    )
}

macro_rules! ret_on_end {
    ($e:expr) => (
        match $e {
            End => return End,
            o => o
        }
    )
}

macro_rules! cancel {
    ($m:ident; $r:expr) => ({$m.cancel(); $r})
}

pub mod config;

mod block;
mod inline;

// Cursor employs inner mutability to support RAII marks.
// Parser employs inner mutability as a consequence of this.

struct Cursor<'a> {
    buf: &'a [u8],
    pos: Cell<usize>
}

impl<'a> Deref for Cursor<'a> {
    type Target = u8;

    #[inline]
    fn deref(&self) -> &u8 {
        &self.buf[self.pos.get()]
    }
}

impl<'a> Cursor<'a> {
    fn new(buf: &[u8]) -> Cursor {
        Cursor {
            buf: buf,
            pos: Cell::new(0)
        }
    }

    #[inline]
    fn available(&self) -> bool { self.pos.get() < self.buf.len() }
    
    // TODO: rename to unsafe_advance? it does not check for buffer end
    #[inline]
    fn advance(&self, n: usize) { self.pos.modify(|p| p + n); }

    #[inline]
    fn retract(&self, n: usize) { self.pos.modify(|p| if n > p { 0 } else { p - n }); }

    #[inline]
    fn next(&self) -> bool {
        if self.available() {
            self.advance(1);
            true
        } else {
            false
        }
    }

    #[inline]
    fn prev(&self) { self.retract(1); }

    #[inline]
    fn current_byte(&self) -> Option<u8> {
        if self.available() { Some(**self) }
        else { None }
    }

    #[inline]
    fn peek_prev(&self) -> u8 {
        self.buf[self.pos.get()-1]
    }

    #[inline]
    fn peek_before_prev(&self) -> u8 {
        self.buf[self.pos.get()-2]
    }

    #[inline]
    fn peek_before_prev_opt(&self) -> Option<u8> {
        let pos = self.pos.get();
        if pos > 1 { Some(self.buf[pos-2]) } else { None }
    }

    #[inline]
    fn next_byte(&self) -> Option<u8> { 
        if self.available() {
            let r = **self; 
            self.advance(1);
            Some(r)
        } else {
            None
        }
    }

    #[inline]
    fn prev_byte(&self) -> u8 { self.retract(1); **self }

    #[inline]
    fn phantom_mark(&self) -> PhantomMark {
        PhantomMark { pos: self.pos.get() }
    }

    #[inline]
    fn phantom_mark_at_prev(&self) -> PhantomMark {
        PhantomMark { pos: self.pos.get()-1 }
    }

    #[inline]
    fn valid(&self, pm: PhantomMark) -> bool {
        pm.pos <= self.buf.len()
    }

    #[inline]
    fn mark(&self) -> Mark { 
        Mark { cur: self, pos: self.pos.get(), cancelled: false }
    }

    #[inline]
    fn slice(&self, left: PhantomMark, right: PhantomMark) -> &'a [u8] {
        &self.buf[left.pos..right.pos]
    }

    #[inline]
    fn slice_to_now_from(&self, pm: PhantomMark) -> &'a [u8] {
        &self.buf[pm.pos..self.pos.get()]
    }

    #[inline]
    fn slice_until_now_from(&self, pm: PhantomMark) -> &'a [u8] {
        &self.buf[pm.pos..self.pos.get()-1]
    }
}

#[derive(Copy, PartialEq, Eq)]
struct PhantomMark {
    pos: usize
}

struct Mark<'b, 'a: 'b> {
    cur: &'b Cursor<'a>,
    pos: usize,
    cancelled: bool
}

#[unsafe_destructor]
impl<'b, 'a> Drop for Mark<'b, 'a> {
    fn drop(&mut self) {
        if !self.cancelled {
            self.cur.pos.set(self.pos);
        }
    }
}

impl<'b, 'a> Mark<'b, 'a> {
    #[inline]
    fn cancel(mut self) { self.cancelled = true; }

    #[inline]
    fn reset(self) {}  // just invoke the destructor
}

pub struct MarkdownParser<'a> {
    cur: Cursor<'a>,
    event_queue: RefCell<VecDeque<Block>>,
    config: MarkdownConfig,
    link_map: Option<LinkMap>
}

// public methods
impl<'a> MarkdownParser<'a> {
    #[inline]
    pub fn new(buffer: &[u8]) -> MarkdownParser {
        MarkdownParser {
            cur: Cursor::new(buffer),
            event_queue: RefCell::new(VecDeque::new()),
            config: MarkdownConfig::default(),
            link_map: Some(HashMap::new())
        }
    }

    #[inline]
    pub fn with_config(mut self, config: MarkdownConfig) -> MarkdownParser<'a> {
        self.config = config;
        self
    }

    #[inline]
    pub fn read_all(self) -> Document {
        self.collect()
    }
}

impl<'a> Iterator for MarkdownParser<'a> {
    type Item = Block;

    fn next(&mut self) -> Option<Block> { 
        let front = self.event_queue.borrow_mut().pop_front();
        front.or_else(|| self.parse_block().to_option())
    }
}

// private methods
impl<'a> MarkdownParser<'a> {
    fn fork<'b>(&self, buffer: &'b [u8]) -> MarkdownParser<'b> {
        MarkdownParser {
            cur: Cursor::new(buffer),
            event_queue: RefCell::new(VecDeque::new()),
            config: self.config,
            link_map: None
        }
    }

    fn try_parse_empty_line(&self) -> ParseResult<()> {
        let m = self.cur.mark();
        loop {
            match opt_ret_end!(self.cur.next_byte()) {
                b' ' => {}
                b'\n' => { m.cancel(); return Success(()) }
                _ => return NoParse
            }
        }
    }

    fn try_skip_initial_spaces(&self) -> ParseResult<()> {
        let mut n: u8 = 0;
        let m = self.cur.mark();
        while self.cur.available() {
            if n >= 4 {
                return NoParse;
            }
            match *self.cur {
                b' ' => { n += 1; self.cur.next(); }  // increase counter and continue
                _ => { m.cancel(); return Success(()); },  // not a space and less than 4 spaces
            }
        }
        End
    }

    fn try_read_char(&self, expected: u8) -> ParseResult<()> {
        match self.cur.next_byte() {
            Some(c) if c == expected => Success(()),
            Some(_) => { self.cur.prev(); NoParse },
            None => End
        }
    }

    fn lookahead_chars(&self, mut n: usize, c: u8) -> bool {
        let _m = self.cur.mark();
        while n > 0 && self.cur.available() {
            match *self.cur {
                cc if cc == c => { self.cur.next(); n -= 1; }
                _ => break
            }
        }
        n == 0
    }

    fn read_line_to(&self, dest: &mut Vec<u8>) -> ParseResult<()> {
        if !self.cur.available() { return End }

        while {
            let c = *self.cur; self.cur.next();
            dest.push(c);

            if c == b'\n' {
                return Success(());
            }
            
            self.cur.available() 
        } {}
        Success(())
    }

    fn read_line(&self) -> ParseResult<()> {
        if !self.cur.available() { return End }

        while {
            let c = *self.cur; self.cur.next();

            if c == b'\n' {
                return Success(())
            }

            self.cur.available()
        } {}
        Success(())
    }

    fn parse<M: ByteMatcher>(&self, mut m: M) -> ParseResult<&'a [u8]> {
        if !self.cur.available() { return End }

        let pm = self.cur.phantom_mark();
        while {
            let c = *self.cur;

            if m.matches(c) {
                self.cur.next();
            } else {
                return match self.cur.slice_until_now_from(pm) {
                    s if s.is_empty() => NoParse,
                    s                 => Success(s)
                };
            }

            self.cur.available()
        } {}
        Success(self.cur.slice_until_now_from(pm))
    }

    fn skip<M: ByteMatcher>(&self, mut m: M) -> ParseResult<()> {
        if !self.cur.available() { return End }

        while {
            let c = *self.cur;

            if m.matches(c) {
                self.cur.next();
            } else {
                return Success(());
            }

            self.cur.available()
        } {}
        Success(())
    }

    #[inline]
    fn skip_spaces(&self) -> ParseResult<()> { 
        self.skip(b' ')
    }

    #[inline]
    fn skip_spaces_and_newlines(&self) -> ParseResult<()> { 
        self.skip([b' ', b'\n'].as_slice())
    }

    #[inline]
    fn fix_links<F: FixLinks>(&self, mut fl: F) -> F {
        fl.fix_links_opt(self.link_map.as_ref());
        fl
    }

    #[inline]
    fn enqueue_event(&self, block: Block) {
        self.event_queue.borrow_mut().push_back(block)
    }
}

trait CharOps {
    fn is_emphasis(self) -> bool;
    fn is_code(self) -> bool;
}

impl CharOps for u8 {
    #[inline]
    fn is_emphasis(self) -> bool {
        self == b'*' || self == b'_'
    }

    #[inline]
    fn is_code(self) -> bool {
        self == b'`' || self == b'`'
    }
}

trait OptionOps<T> {
    fn to_parse_result(self) -> ParseResult<T>;
}

enum ParseResult<T> {
    Success(T),
    NoParse,
    End
}

impl<T> ParseResult<T> {
    fn or_else<F>(self, f: F) -> ParseResult<T> where F: FnOnce() -> ParseResult<T> {
        match self {
            Success(r) => Success(r),
            End => End,
            NoParse => f()
        }
    }

    fn map<U, F>(self, f: F) -> ParseResult<U> where F: FnOnce(T) -> U {
        match self {
            Success(r) => Success(f(r)),
            End => End,
            NoParse => NoParse
        }
    }

    #[inline]
    fn unwrap(self) -> T {
        match self {
            Success(r) => r,
            End => panic!("End unwrap"),
            NoParse => panic!("NoParse unwrap")
        }
    }

    #[inline]
    fn is_success(&self) -> bool {
        match *self {
            Success(_) => true,
            _ => false
        }
    }

    #[inline]
    fn is_end(&self) -> bool {
        match *self {
            End => true,
            _ => false
        }
    }

    fn to_option(self) -> Option<T> {
        match self {
            Success(r) => Some(r),
            End => None,
            NoParse => panic!("programming error, NoParse is converted to result")
        }
    }
}
