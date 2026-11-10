//! Book edits: 1.17 sends the edited book as an item stack and a signing
//! flag, 1.17.1 the pages and an optional title, both with the inventory
//! slot the book is in.
use super::OLDER;
use crate::{
    data,
    nbt::{Compound, Nbt},
    wire::{Reader, Writer, invalid},
};
use std::io;

/// 1.17.1 servers read at most this many pages of at most 8192 characters,
/// and titles of at most 128.
const MAX_PAGES: usize = 200;
const MAX_PAGE: usize = 8192;
const MAX_TITLE: usize = 128;

/// The 1.17 item stack of a writable book holding `pages`, with the title
/// and author of a book being signed.
fn writable_book(pages: Vec<String>, signed: Option<(String, &str)>) -> io::Result<Vec<u8>> {
    let id = data::version(OLDER)
        .expect_registry("item")
        .id("writable_book")
        .ok_or_else(|| invalid("1.17 has no writable book"))?;
    let mut tag = Compound::new();
    // A writable book always has a page; older servers reject one without.
    let pages = if pages.is_empty() {
        vec![String::new()]
    } else {
        pages
    };
    tag.insert(
        "pages",
        pages.into_iter().map(Nbt::String).collect::<Vec<_>>(),
    );
    if let Some((title, author)) = signed {
        tag.insert("title", title);
        tag.insert("author", author);
    }
    let mut w = Writer::new();
    w.bool(true).varint(id).i8(1);
    Nbt::Compound(tag).write(&mut w, true);
    Ok(w.into_inner())
}

/// A 1.17.1 `edit_book` body as 1.17 sends it. `author` is the player's
/// name, which servers expect on a book being signed.
pub fn downgrade(data: &[u8], author: &str) -> io::Result<Vec<u8>> {
    let mut r = Reader::new(data);
    let slot = r.varint()?;
    let pages = (0..r.count()?)
        .map(|_| r.string(MAX_PAGE).map(str::to_owned))
        .collect::<io::Result<Vec<_>>>()?;
    let title = if r.bool()? {
        Some(r.string(MAX_TITLE)?.to_owned())
    } else {
        None
    };
    r.finish()?;
    if pages.len() > MAX_PAGES {
        return Err(invalid("too many book pages"));
    }
    let signing = title.is_some();
    let mut w = Writer::new();
    w.bytes(&writable_book(pages, title.map(|title| (title, author)))?)
        .bool(signing)
        .varint(slot);
    Ok(w.into_inner())
}

/// Cut `text` to at most `max` characters, as a server would refuse more.
fn truncate(mut text: String, max: usize) -> String {
    if text.encode_utf16().count() > max {
        let mut units = 0;
        let end = text
            .char_indices()
            .find(|(_, c)| {
                units += c.len_utf16();
                units > max
            })
            .map_or(text.len(), |(index, _)| index);
        text.truncate(end);
    }
    text
}

/// A 1.17 `edit_book` body as 1.17.1 sends it: the pages and title are
/// taken out of the book's tag and cut to what 1.17.1 servers accept.
pub fn upgrade(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut r = Reader::new(data);
    let tag = if r.bool()? {
        r.varint()?;
        r.i8()?;
        Nbt::read(&mut r, true)?
    } else {
        None
    };
    let signing = r.bool()?;
    let slot = r.varint()?;
    r.finish()?;
    let tag = tag.as_ref().and_then(Nbt::as_compound);
    let pages: Vec<String> = tag
        .and_then(|tag| tag.list("pages"))
        .unwrap_or_default()
        .iter()
        .filter_map(Nbt::as_str)
        .take(MAX_PAGES)
        .map(|page| truncate(page.to_owned(), MAX_PAGE))
        .collect();
    let mut w = Writer::new();
    w.varint(slot).varint(pages.len() as i32);
    for page in &pages {
        w.string(page);
    }
    w.bool(signing);
    if signing {
        let title = tag.and_then(|tag| tag.str("title")).unwrap_or_default();
        w.string(&truncate(title.to_owned(), MAX_TITLE));
    }
    Ok(w.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_and_signatures_convert_both_ways() {
        let mut new = Writer::new();
        new.varint(3)
            .varint(2)
            .string("one")
            .string("two")
            .bool(false);
        let old = downgrade(&new.buf, "Steve").unwrap();
        assert_eq!(upgrade(&old).unwrap(), new.buf);

        let mut signed = Writer::new();
        signed
            .varint(40)
            .varint(1)
            .string("page")
            .bool(true)
            .string("Title");
        let old = downgrade(&signed.buf, "Steve").unwrap();
        let mut r = Reader::new(&old);
        assert!(r.bool().unwrap());
        assert_eq!(
            r.varint().unwrap(),
            data::version(OLDER)
                .expect_registry("item")
                .id("writable_book")
                .unwrap()
        );
        assert_eq!(r.i8().unwrap(), 1);
        let tag = Nbt::read(&mut r, true).unwrap().unwrap();
        let tag = tag.as_compound().unwrap();
        assert_eq!(tag.str("title"), Some("Title"));
        assert_eq!(tag.str("author"), Some("Steve"));
        assert!(r.bool().unwrap(), "signing");
        assert_eq!(r.varint().unwrap(), 40);
        r.finish().unwrap();
        assert_eq!(upgrade(&old).unwrap(), signed.buf);
    }

    #[test]
    fn empty_books_and_long_text_fit_the_receiver() {
        let mut new = Writer::new();
        new.varint(0).varint(0).bool(false);
        let old = downgrade(&new.buf, "").unwrap();
        let mut expected = Writer::new();
        expected.varint(0).varint(1).string("").bool(false);
        assert_eq!(upgrade(&old).unwrap(), expected.buf);

        // A 1.17 client's empty hand: no book, no pages.
        assert_eq!(upgrade(&[0, 0, 5]).unwrap(), [5, 0, 0]);
        assert_eq!(truncate("é".repeat(10), 4), "éééé");
        assert_eq!(truncate("ab".into(), 4), "ab");
    }

    #[test]
    fn malformed_edits_fail() {
        assert!(downgrade(&[0, 1], "").is_err());
        assert!(downgrade(&[0, 0, 0, 0], "").is_err());
        assert!(upgrade(&[1, 0x80]).is_err());
        assert!(upgrade(&[0, 0, 0, 0]).is_err());
    }
}
