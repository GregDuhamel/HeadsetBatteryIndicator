//! The dongle's report: the request that asks, the frame that answers, and
//! the messages found in it.
//!
//! The layout is described in the [module documentation](super): a stream of
//! small messages, oldest first, with a count of the bytes written since the
//! report was last fetched.

use std::io;

use hidraw::Device;

/// Every report the dongle exchanges is this long, report ID included.
pub(super) const MSG_SIZE: usize = 62;

/// Report ID of the dongle's answers.
const REPLY_REPORT_ID: u8 = 0x07;

/// "What is the battery level?", on output report `0x06`, relayed to the
/// headset (the `0x80` in third position; `0x00` addresses the dongle itself).
pub(super) const BATTERY_REQUEST: [u8; 9] = [0x06, 0x07, 0x80, 0x05, 0x5a, 0x03, 0x00, 0xd6, 0x0c];

/// Offset of the count of fresh bytes in an input report.
const FRESH_LEN_OFFSET: usize = 1;

/// Offset of the first message in an input report.
const PAYLOAD_OFFSET: usize = 3;

/// Header of the battery answer: message type `0x5d`, five payload bytes,
/// echoing the request (`d6 0c`) with a zero status. The level follows.
const BATTERY_ANSWER: [u8; 7] = [0x5d, 0x05, 0x00, 0xd6, 0x0c, 0x00, 0x00];

/// Header of a link event. The byte that follows is `01` when the headset is
/// linked and `00` when it is gone; then come a `01` and the headset's address.
const LINK_EVENT: [u8; 7] = [0x5d, 0x0e, 0x00, 0xb1, 0x2c, 0x00, 0x02];

/// Something the dongle's report stream said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Message {
    /// The headset's link came up or went down.
    Link(bool),
    /// The battery level, in percent.
    Battery(u8),
}

/// The part of an input report written since it was last fetched.
fn fresh(frame: &[u8]) -> &[u8] {
    let len = frame.get(FRESH_LEN_OFFSET).copied().map_or(0, usize::from);
    let end = (PAYLOAD_OFFSET + len).min(frame.len());
    frame.get(PAYLOAD_OFFSET..end).unwrap_or_default()
}

/// Everything the fresh part of an input report says, oldest first.
///
/// Matching on the full message headers, rather than on the command bytes
/// alone, is what keeps an acknowledgement (which echoes `d6 0c` too) from
/// being read as a level. A message cut off by the end of the buffer is
/// skipped, and so is a level above 100.
pub(super) fn messages(frame: &[u8]) -> Vec<Message> {
    let data = fresh(frame);
    (0..data.len())
        .filter_map(|at| {
            let rest = &data[at..];
            if let Some(value) = rest
                .strip_prefix(&BATTERY_ANSWER[..])
                .and_then(<[u8]>::first)
            {
                (*value <= 100).then_some(Message::Battery(*value))
            } else {
                rest.strip_prefix(&LINK_EVENT[..])
                    .and_then(<[u8]>::first)
                    .map(|state| Message::Link(*state != 0))
            }
        })
        .collect()
}

/// Fetches the current value of the dongle's answer report (`HIDIOCGINPUT`,
/// [`Device::get_input`]).
///
/// The dongle never pushes its answers on the interrupt endpoint - a plain
/// `read()` on the hidraw node sees nothing - so they have to be fetched with a
/// `GET_REPORT` control transfer, which hidraw only exposes through that ioctl.
/// The report is always [`MSG_SIZE`] bytes, so the buffer is that large and
/// the count the kernel returns is not needed: a shorter answer would leave
/// zeros behind it, and zeros say nothing (see [`messages`]).
pub(super) fn fetch(device: &Device) -> io::Result<[u8; MSG_SIZE]> {
    // Byte 0 tells the kernel which report is wanted; the report overwrites it.
    let mut frame = [0u8; MSG_SIZE];
    frame[0] = REPLY_REPORT_ID;
    device.get_input(&mut frame)?;
    Ok(frame)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// Captured from a Maxwell Xbox dongle right after a battery request, with
    /// the headset at 92%. Sixteen fresh bytes - the acknowledgement and the
    /// answer - followed by what earlier exchanges left behind, including an
    /// older answer that says 91%.
    const FRESH_FRAME: [u8; MSG_SIZE] = [
        0x07, 0x10, 0x80, 0x05, 0x5b, 0x03, 0x00, 0xd6, 0x0c, 0x00, 0x05, 0x5d, 0x05, 0x00, 0xd6,
        0x0c, 0x00, 0x00, 0x5c, 0x07, 0x31, 0x2e, 0x30, 0x2e, 0x31, 0x2e, 0x37, 0x34, 0x05, 0x00,
        0xd6, 0x0c, 0x00, 0x00, 0x5b, 0x05, 0x5b, 0x03, 0x00, 0xd6, 0x0c, 0x00, 0x05, 0x5d, 0x05,
        0x00, 0xd6, 0x0c, 0x00, 0x00, 0x5b, 0x00, 0x0c, 0x00, 0x00, 0x5b, 0x05, 0x5b, 0x03, 0x00,
        0xd6, 0x0c,
    ];

    /// The same report fetched a second time: nothing new, so the count is
    /// zero, yet the buffer still holds every old answer.
    pub(in crate::maxwell) const STALE_FRAME: [u8; MSG_SIZE] = [
        0x07, 0x00, 0x80, 0x05, 0x5b, 0x03, 0x00, 0xd6, 0x0c, 0x00, 0x05, 0x5d, 0x05, 0x00, 0xd6,
        0x0c, 0x00, 0x00, 0x5c, 0x07, 0x31, 0x2e, 0x30, 0x2e, 0x31, 0x2e, 0x37, 0x34, 0x05, 0x00,
        0xd6, 0x0c, 0x00, 0x00, 0x5b, 0x05, 0x5b, 0x03, 0x00, 0xd6, 0x0c, 0x00, 0x05, 0x5d, 0x05,
        0x00, 0xd6, 0x0c, 0x00, 0x00, 0x5b, 0x00, 0x0c, 0x00, 0x00, 0x5b, 0x05, 0x5b, 0x03, 0x00,
        0xd6, 0x0c,
    ];

    /// A frame holding `stream` as its only fresh content.
    pub(in crate::maxwell) fn frame_with(stream: &[u8]) -> [u8; MSG_SIZE] {
        let mut frame = [0u8; MSG_SIZE];
        frame[0] = REPLY_REPORT_ID;
        frame[FRESH_LEN_OFFSET] = u8::try_from(stream.len()).unwrap();
        frame[PAYLOAD_OFFSET..PAYLOAD_OFFSET + stream.len()].copy_from_slice(stream);
        frame
    }

    pub(in crate::maxwell) fn hex(text: &str) -> Vec<u8> {
        text.split_whitespace()
            .map(|byte| u8::from_str_radix(byte, 16).unwrap())
            .collect()
    }

    /// What the dongle said, unprompted, when the headset was switched off.
    pub(in crate::maxwell) const POWER_OFF: &str =
        "05 5d 0e 00 b1 2c 00 02 00 01 8d 90 9b 67 89 c2 ff 00 05 5c 03 00 80 2c 01";

    /// And when it was switched back on: the link event, then the level.
    pub(in crate::maxwell) const POWER_ON: &str = "05 5c 03 00 80 2c 01 05 5d 0e 00 b1 2c 00 02 01 01 8d 90 9b 67 89 \
                            c2 80 01 05 5c 03 00 80 2c 03 05 5b 03 00 d6 0c 00 05 5d 05 00 d6 \
                            0c 00 00 63";

    #[test]
    fn reads_the_level_out_of_a_real_frame() {
        assert_eq!(messages(&FRESH_FRAME), [Message::Battery(92)]);
    }

    #[test]
    fn leftovers_from_earlier_exchanges_are_not_an_answer() {
        // Without the fresh-byte count this frame reads 92% for ever, which is
        // what HeadsetControl shows for a headset that is switched off.
        assert_eq!(messages(&STALE_FRAME), []);
    }

    #[test]
    fn the_dongle_announces_the_headset_going_and_coming() {
        assert_eq!(
            messages(&frame_with(&hex(POWER_OFF))),
            [Message::Link(false)]
        );
        assert_eq!(
            messages(&frame_with(&hex(POWER_ON))),
            [Message::Link(true), Message::Battery(99)]
        );
    }

    #[test]
    fn an_acknowledgement_is_not_mistaken_for_an_answer() {
        // The acknowledgement (type 5b) echoes `d6 0c` too. Followed by
        // padding it reads `d6 0c 00 00 00`, which a looser match turns into a
        // bogus 0% - the glitch HeadsetControl produces.
        let frame = frame_with(&hex("05 5b 03 00 d6 0c 00 00 00 00"));
        assert_eq!(messages(&frame), []);
    }

    #[test]
    fn nonsense_is_ignored() {
        assert_eq!(messages(&[0u8; MSG_SIZE]), []);
        assert_eq!(messages(&[]), []);
        assert_eq!(messages(&[REPLY_REPORT_ID]), []);
        // A level above 100.
        assert_eq!(
            messages(&frame_with(&hex("05 5d 05 00 d6 0c 00 00 ff"))),
            []
        );
        assert_eq!(
            messages(&frame_with(&hex("05 5d 05 00 d6 0c 00 00 64"))),
            [Message::Battery(100)]
        );
        // An answer cut off before its level byte.
        assert_eq!(messages(&frame_with(&hex("05 5d 05 00 d6 0c 00 00"))), []);
    }

    #[test]
    fn a_fresh_count_larger_than_the_frame_does_not_panic() {
        let mut frame = FRESH_FRAME;
        frame[FRESH_LEN_OFFSET] = 0xff;
        // Clamped to the buffer: everything then counts, leftovers included.
        assert_eq!(messages(&frame).last(), Some(&Message::Battery(91)));
    }
}
