use {TextUnit};

use std::str::Chars;

pub(crate) struct Ptr<'s> {
    text: &'s str,
    len: TextUnit,
}

impl<'s> Ptr<'s> {
    pub fn new(text: &'s str) -> Ptr<'s> {
        Ptr { text, len: TextUnit::new(0) }
    }

    pub fn into_len(self) -> TextUnit {
        self.len
    }

    pub fn next(&self) -> Option<char> {
        self.chars().next()
    }

    pub fn nnext(&self) -> Option<char> {
        let mut chars = self.chars();
        chars.next()?;
        chars.next()
    }

    pub fn bump(&mut self) -> Option<char> {
        let ch = self.chars().next()?;
        self.len += TextUnit::len_of_char(ch);
        Some(ch)
    }

    fn chars(&self) -> Chars {
        self.text[self.len.0 as usize ..].chars()
    }
}
