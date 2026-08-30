use core::panic;
use std::ffi::{self, OsStr};

use crate::{file_handle::{FileHan, FileHandleOps}, util};

pub enum EventType {
    Create(usize),
    Delete(usize),
    Rename(usize, usize),
    // fanotify groups its events if all other information is the same.
    // Rename doesn't group with anything else, but create & deletes can.
    // We can't figure out which came first. but for a file DB, it doesn't matter!
    #[allow(unused)]
    CreateDelete(usize),
}

pub struct Event<'a> {
    buf: &'a [u8],
    fid: usize,
    pub r#type: EventType
}

impl <'a> Event<'a> {
    pub fn from_slice(buf: &'a [u8]) -> (Self, usize) {
        let metadata: libc::fanotify_event_metadata = util::read_as_type(buf);
        let event_len = metadata.event_len as usize;

        let mut ptr = size_of::<libc::fanotify_event_metadata>();

        let mut fid = None;
        let mut dfid = None;
        let mut old_dfid = None;
        let mut new_dfid = None;

        while ptr < event_len {
            let header: libc::fanotify_event_info_header = util::read_as_type(&buf[ptr..]);
            match header.info_type {
                libc::FAN_EVENT_INFO_TYPE_FID           => fid = Some(ptr),
                libc::FAN_EVENT_INFO_TYPE_DFID_NAME     => dfid = Some(ptr),
                libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME => old_dfid = Some(ptr),
                libc::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME => new_dfid = Some(ptr),
                _ => panic!("Unexpected fanotify record: {}", header.info_type)
            }
            ptr += header.len as usize;
        }

        let stripped_mask = metadata.mask & (libc::FAN_CREATE | libc::FAN_DELETE | libc::FAN_RENAME);

        // Using | for 'matches both' makes sense
        // but why do you keep doing that EVEN WHEN I PUT THE WHOLE THING IN PARENTHESIS???
        const CREATE_AND_DELETE: u64 = libc::FAN_CREATE | libc::FAN_DELETE;

        let event_type = match stripped_mask {
            libc::FAN_CREATE => EventType::Create(
                dfid.expect("fanotify create event missing dfid")
            ),
            libc::FAN_DELETE => EventType::Delete(
                dfid.expect("fanotify delete event missing dfid")
            ),
            libc::FAN_RENAME => EventType::Rename(
                old_dfid.expect("fanotify rename event missing old_dfid"),
                new_dfid.expect("fanotify rename event missing new_dfid")
            ),
            CREATE_AND_DELETE => EventType::CreateDelete(
                dfid.expect("fanotify create & delete event missing dfid")
            ),
            _ => panic!("Unexpected fanotify metadata mask: {}", metadata.mask)
        };

        (Event {
            buf: buf,
            fid: fid.expect("fanotify event missing fid"),
            r#type: event_type
        }, event_len)
    }
}

fn get_fh(buf: &[u8]) -> &FileHan {
    let buf_after_fid = &buf[size_of::<libc::fanotify_event_info_fid>()..];
    FileHan::read_from_buf(buf_after_fid)
        .expect("Invalid file handle")
}

fn get_name(buf: &[u8]) -> &OsStr {
    let name_start = size_of::<libc::fanotify_event_info_fid>() + get_fh(buf).size();

    let c_str = ffi::CStr::from_bytes_until_nul(&buf[name_start..])
        .expect("Failed to get filename");
    unsafe {OsStr::from_encoded_bytes_unchecked(c_str.to_bytes())}
}

impl Event<'_> {
    pub fn fh  (&self)              -> &FileHan {get_fh  (&self.buf[self.fid..])}
    pub fn p_fh(&self, dfid: usize) -> &FileHan {get_fh  (&self.buf[dfid..])}
    pub fn name(&self, dfid: usize) -> &OsStr   {get_name(&self.buf[dfid..])}
}
