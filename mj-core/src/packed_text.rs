//! Immutable transcript text; compression never changes its serialized form.
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, cell::RefCell};

thread_local! {
    static CODEC: RefCell<(zstd::bulk::Compressor<'static>, zstd::bulk::Decompressor<'static>)> =
        RefCell::new((zstd::bulk::Compressor::new(3).expect("zstd compressor"),
                      zstd::bulk::Decompressor::new().expect("zstd decompressor")));
}

#[derive(Debug, Clone)]
pub struct PackedText(Repr);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Repr {
    Plain(String),
    Zstd { frame: Box<[u8]>, len: usize },
}

impl PackedText {
    /// Keep an accumulating terminal plain until its final observation.
    pub fn plain(text: String) -> Self {
        Self(Repr::Plain(text))
    }
    pub fn text(&self) -> Cow<'_, str> {
        match &self.0 {
            Repr::Plain(text) => Cow::Borrowed(text),
            Repr::Zstd { frame, len } => Cow::Owned(CODEC.with(|codec| {
                String::from_utf8(
                    codec
                        .borrow_mut()
                        .1
                        .decompress(frame, *len)
                        .expect("internally generated zstd frame"),
                )
                .expect("packed UTF-8")
            })),
        }
    }
    pub fn into_text(self) -> String {
        match self.0 {
            Repr::Plain(text) => text,
            _ => self.text().into_owned(),
        }
    }
    pub fn len(&self) -> usize {
        match &self.0 {
            Repr::Plain(text) => text.len(),
            Repr::Zstd { len, .. } => *len,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
impl Default for PackedText {
    fn default() -> Self {
        Self::plain(String::new())
    }
}
impl From<String> for PackedText {
    fn from(text: String) -> Self {
        let frame = CODEC
            .with(|codec| codec.borrow_mut().0.compress(text.as_bytes()))
            .expect("compress transcript text");
        if frame.len() < text.len() {
            Self(Repr::Zstd {
                frame: frame.into_boxed_slice(),
                len: text.len(),
            })
        } else {
            Self::plain(text)
        }
    }
}
impl From<&str> for PackedText {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}
impl PartialEq for PackedText {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0 || self.text() == other.text()
    }
}
impl Eq for PackedText {}
impl Serialize for PackedText {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.text().serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for PackedText {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(Into::into)
    }
}
