//! A 1.21.8 client (772) on backends that declare other protocols.
use super::*;

const CLIENT: i32 = 772;
const SET_SLOT: i32 = 0x14; // Both protocols.
const CREATIVE_SLOT: i32 = 0x37;
const UPDATE_TAGS: i32 = 0x0d; // Configuration.

fn set_slot(item: i32) -> Packet {
    let mut data = vec![0, 1]; // Container and state ID.
    data.extend(36i16.to_be_bytes());
    data.push(1); // Count.
    write_varint(item, &mut data);
    data.extend([0, 0]); // No component changes.
    Packet::new(SET_SLOT, data)
}

fn creative_slot(item: i32) -> Packet {
    let mut data = 36i16.to_be_bytes().to_vec();
    data.push(1);
    write_varint(item, &mut data);
    data.extend([0, 0]);
    Packet::new(CREATIVE_SLOT, data)
}

fn item_tags(item: i32) -> Packet {
    let mut data = vec![1];
    write_string("minecraft:item", &mut data);
    data.push(1);
    write_string("minecraft:test", &mut data);
    data.push(1);
    write_varint(item, &mut data);
    Packet::new(UPDATE_TAGS, data)
}

fn join_game() -> Packet {
    let mut join = super::join_game(1);
    join.id = 0x2b;
    join
}

async fn client_session(
    backend_protocol: Option<i32>,
) -> io::Result<(
    Session<DuplexStream, DuplexStream>,
    DuplexStream,
    DuplexStream,
)> {
    let (mut client, input) = tokio::io::duplex(65536);
    let handshake = Handshake {
        protocol: CLIENT,
        address: "play.test".into(),
        port: 25565,
        next_state: NextState::Login,
    };
    Codec::default()
        .write(&mut client, &handshake.packet())
        .await?;
    let mut session = Session::accept(input).await?;
    let (server, backend) = tokio::io::duplex(65536);
    session
        .connect_backend_as(
            server,
            backend_protocol.map(|number| ProtocolVersion::new(number).unwrap()),
        )
        .await?;
    Ok((session, client, backend))
}

#[tokio::test]
async fn older_backend_is_translated_and_a_switch_to_the_client_protocol_stops_translating() {
    timeout(Duration::from_secs(3), async {
        let codec = Codec::default();
        let (mut session, mut client, mut backend) = client_session(Some(771)).await.unwrap();
        let handshake = Handshake::decode(&receive(&mut backend, codec).await).unwrap();
        assert_eq!(handshake.protocol, 771, "the backend sees its own protocol");
        codec.write(&mut client, &login_start()).await.unwrap();
        session.forward().await.unwrap();
        assert_eq!(receive(&mut backend, codec).await, login_start());
        codec.write(&mut backend, &login_success()).await.unwrap();
        session.forward().await.unwrap();
        assert_eq!(receive(&mut client, codec).await, login_success());
        codec.write(&mut client, &Packet::empty(3)).await.unwrap();
        session.forward().await.unwrap();
        assert_eq!(receive(&mut backend, codec).await, Packet::empty(3));

        // Configuration: item tags are renumbered for the newer client.
        codec.write(&mut backend, &item_tags(1300)).await.unwrap();
        session.forward().await.unwrap();
        assert_eq!(receive(&mut client, codec).await, item_tags(1301));
        codec.write(&mut backend, &Packet::empty(3)).await.unwrap();
        session.forward().await.unwrap();
        assert_eq!(receive(&mut client, codec).await, Packet::empty(3));
        codec.write(&mut client, &Packet::empty(3)).await.unwrap();
        session.forward().await.unwrap();
        assert_eq!(receive(&mut backend, codec).await, Packet::empty(3));
        codec.write(&mut backend, &join_game()).await.unwrap();
        session.forward().await.unwrap();
        assert_eq!(receive(&mut client, codec).await, join_game());
        assert!(session.can_switch());

        // Play in both directions, including an item only the client has.
        codec.write(&mut backend, &set_slot(1300)).await.unwrap();
        session.forward().await.unwrap();
        assert_eq!(receive(&mut client, codec).await, set_slot(1301));
        codec
            .write(&mut client, &creative_slot(1256))
            .await
            .unwrap();
        session.forward().await.unwrap();
        assert_eq!(receive(&mut backend, codec).await, creative_slot(1253));

        // Switch to a backend on the client's own protocol.
        let (replacement, mut second) = tokio::io::duplex(65536);
        let switch =
            session.connect_backend_as(replacement, Some(ProtocolVersion::new(CLIENT).unwrap()));
        let peers = async {
            let handshake = Handshake::decode(&receive(&mut second, codec).await).unwrap();
            assert_eq!(handshake.protocol, CLIENT);
            assert_eq!(receive(&mut second, codec).await, login_start());
            codec.write(&mut second, &login_success()).await.unwrap();
            assert_eq!(receive(&mut second, codec).await, Packet::empty(3));
            assert_eq!(receive(&mut client, codec).await, Packet::empty(0x6f));
            codec
                .write(&mut client, &Packet::empty(0x0f))
                .await
                .unwrap();
            assert_eq!(receive(&mut client, codec).await, Packet::new(8, vec![0]));
        };
        let (result, ()) = tokio::join!(switch, peers);
        result.unwrap();
        codec.write(&mut second, &item_tags(1301)).await.unwrap();
        session.forward().await.unwrap();
        assert_eq!(receive(&mut client, codec).await, item_tags(1301));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn untranslatable_backend_protocols_are_rejected_before_connecting() {
    let error = client_session(Some(774)).await.err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    // The same protocol, declared or not, needs no translation.
    assert!(client_session(Some(CLIENT)).await.is_ok());
    assert!(client_session(None).await.is_ok());
}
