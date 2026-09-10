use std::ffi::{self, OsStr};

use crate::{file_handle::{FileHan, FileHandleOps}, util};

pub fn read_metadata(buf: &[u8]) -> libc::fanotify_event_metadata {
    util::read_as_type(buf)
}

fn read_fh(buf: &[u8]) -> &FileHan {
    let buf_after_fid = &buf[size_of::<libc::fanotify_event_info_fid>()..];
    FileHan::read_from_buf(buf_after_fid)
        .expect("Invalid file handle")
}

fn read_name(buf: &[u8]) -> &OsStr {
    let name_start = size_of::<libc::fanotify_event_info_fid>() + read_fh(buf).size();

    let c_str = ffi::CStr::from_bytes_until_nul(&buf[name_start..])
        .expect("Failed to get filename");
    unsafe {OsStr::from_encoded_bytes_unchecked(c_str.to_bytes())}
}

pub fn read_create_delete<'a>(buf: &'a [u8], metadata: &libc::fanotify_event_metadata)
-> (&'a FileHan, (&'a FileHan, &'a OsStr)) {
    let mut ptr = metadata.metadata_len as usize;
    let mut fid = None;
    let mut dfid_name = None;

    while ptr < metadata.event_len as usize {
        let here = &buf[ptr..];
        let header: libc::fanotify_event_info_header = util::read_as_type(here);

        match header.info_type {
            libc::FAN_EVENT_INFO_TYPE_FID =>
                fid = Some(read_fh(here)),
            libc::FAN_EVENT_INFO_TYPE_DFID_NAME =>
                dfid_name = Some((read_fh(here), read_name(here))),
            _ => {}
        }
        ptr += header.len as usize
    }
    (
        fid.expect("fanotify create/delete event missing FID"),
        dfid_name.expect("fanotify create/delete event missing DFID_NAME")
    )
}

pub fn read_rename<'a>(buf: &'a [u8], metadata: &libc::fanotify_event_metadata)
-> (&'a FileHan, (&'a FileHan, &'a OsStr), (&'a FileHan, &'a OsStr)) {
    let mut ptr = metadata.metadata_len as usize;
    let mut fid = None;
    let mut old_dfid_name = None;
    let mut new_dfid_name = None;

    while ptr < metadata.event_len as usize {
        let here = &buf[ptr..];
        let header: libc::fanotify_event_info_header = util::read_as_type(here);

        match header.info_type {
            libc::FAN_EVENT_INFO_TYPE_FID =>
                fid = Some(read_fh(here)),
            libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME =>
                old_dfid_name = Some((read_fh(here), read_name(here))),
            libc::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME =>
                new_dfid_name = Some((read_fh(here), read_name(here))),
            _ => {}
        }
        ptr += header.len as usize
    }
    (
        fid.expect("fanotify rename event missing FID"),
        old_dfid_name.expect("fanotify rename event missing OLD_DFID_NAME"),
        new_dfid_name.expect("fanotify rename event missing OLD_DFID_NAME"),
    )
}

pub fn read_open<'a>(buf: &'a [u8], metadata: &libc::fanotify_event_metadata)
-> &'a FileHan {
    let mut ptr = metadata.metadata_len as usize;
    let mut fid = None;

    while ptr < metadata.event_len as usize {
        let here = &buf[ptr..];
        let header: libc::fanotify_event_info_header = util::read_as_type(here);

        match header.info_type {
            libc::FAN_EVENT_INFO_TYPE_FID =>
                fid = Some(read_fh(here)),
            // On directories, we get a DFID_NAME(fid, '.')
            libc::FAN_EVENT_INFO_TYPE_DFID if metadata.mask & libc::FAN_ONDIR != 0 =>
                fid = Some(read_fh(here)),
            _ => {}
        }
        ptr += header.len as usize
    }
    fid.expect("fanotify open event missing FID")
}
