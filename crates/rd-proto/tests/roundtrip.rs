use prost::Message;
use rd_proto::v0::{envelope::Msg, Envelope, Hello};

#[test]
fn hello_roundtrip() {
    let original = Envelope {
        req_id: 42,
        msg: Some(Msg::Hello(Hello {
            proto_ver: 0,
            device_name: "controller-dev".to_owned(),
            decode_caps: vec![],
            supports_444: false,
        })),
    };

    let bytes = original.encode_to_vec();
    let decoded = Envelope::decode(bytes.as_slice()).expect("decode");
    assert_eq!(original, decoded);
}
