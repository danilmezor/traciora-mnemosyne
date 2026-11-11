use mnemosyne_runtime_protocol::*;
use std::io::{self, Read};
#[test]
fn binary_frames_preserve_non_json_values_and_opaque_identity() {
    let values = vec![
        HostValue::Number(-0.0),
        HostValue::Number(f64::from_bits(0x7ff8_0000_0000_1234)),
        HostValue::Utf16(vec![0xd800, 0, 0xdc00]),
        HostValue::BigInt {
            negative: true,
            words: vec![0, u64::MAX],
        },
        HostValue::HostObject {
            id: HostObjectId(99),
            class: "TFile".encode_utf16().collect(),
        },
        HostValue::Reference(JsReference {
            id: JsValueId {
                context: 17,
                index: 88,
            },
            kind: ReferenceKind::Symbol,
        }),
    ];
    let message = ParentMessage::HostReply {
        id: 4,
        result: Ok(HostValue::Array(values)),
    };
    let mut wire = Vec::new();
    write_frame(&mut wire, &message).unwrap();
    struct Fragmented<'a>(&'a [u8]);
    impl Read for Fragmented<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            let n = out.len().min(3).min(self.0.len());
            out[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }
    let decoded: ParentMessage = read_frame(&mut Fragmented(&wire)).unwrap();
    let ParentMessage::HostReply {
        id: 4,
        result: Ok(HostValue::Array(decoded)),
    } = decoded
    else {
        panic!("wrong message")
    };
    assert!(matches!(&decoded[0],HostValue::Number(n) if n.to_bits()==(-0.0f64).to_bits()));
    assert!(matches!(&decoded[1],HostValue::Number(n) if n.to_bits()==0x7ff8_0000_0000_1234));
    let ParentMessage::HostReply {
        result: Ok(HostValue::Array(original)),
        ..
    } = message
    else {
        unreachable!()
    };
    assert_eq!(&decoded[2..], &original[2..]);
    assert!(
        matches!(read_frame::<ParentMessage>(&mut &wire[..wire.len()-1]),Err(FrameError::Io(e)) if e.kind()==io::ErrorKind::UnexpectedEof)
    );
}
