use std::fmt;

pub const UNCONNECTED_PING_ID: u8 = 0x01;
pub const UNCONNECTED_PONG_ID: u8 = 0x1C;

#[derive(Clone, Debug, Default)]
pub struct UnconnectedPing {
    pub ping_time: [u8; 8],
    pub id: [u8; 8],
    pub magic: [u8; 16],
    pub pong: PongData,
}

#[derive(Clone, Debug, Default)]
pub struct PongData {
    pub edition: String,
    pub motd: String,
    pub protocol_version: String,
    pub version: String,
    pub players: String,
    pub max_players: String,
    pub server_id: String,
    pub sub_motd: String,
    pub game_type: String,
    pub nintendo_limited: String,
    pub port4: String,
    pub port6: String,
}

impl UnconnectedPing {
    pub fn build(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(UNCONNECTED_PONG_ID);
        out.extend_from_slice(&self.ping_time);
        out.extend_from_slice(&self.id);
        out.extend_from_slice(&self.magic);

        let pong_data = write_pong(&self.pong);
        let pong_len = pong_data.len() as u16;
        out.extend_from_slice(&pong_len.to_be_bytes());
        out.extend_from_slice(pong_data.as_bytes());

        out
    }
}

pub fn offline_pong() -> Vec<u8> {
    UnconnectedPing {
        ping_time: [0; 8],
        id: [0; 8],
        magic: [
            0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34,
            0x56, 0x78,
        ],
        pong: PongData {
            edition: "MCPE".to_string(),
            motd: "phantom §cServer offline".to_string(),
            protocol_version: "390".to_string(),
            version: "1.14.60".to_string(),
            players: "0".to_string(),
            max_players: "0".to_string(),
            server_id: String::new(),
            sub_motd: String::new(),
            game_type: "Creative".to_string(),
            nintendo_limited: "1".to_string(),
            port4: String::new(),
            port6: String::new(),
        },
    }
    .build()
}

pub fn read_unconnected_ping(input: &[u8]) -> Result<UnconnectedPing, String> {
    if input.len() < 35 {
        return Err("packet too short".to_string());
    }

    if input[0] != UNCONNECTED_PONG_ID && input[0] != UNCONNECTED_PING_ID {
        return Err("unsupported packet id".to_string());
    }

    let mut idx = 1usize;

    let mut ping_time = [0u8; 8];
    ping_time.copy_from_slice(&input[idx..idx + 8]);
    idx += 8;

    let mut id = [0u8; 8];
    id.copy_from_slice(&input[idx..idx + 8]);
    idx += 8;

    let mut magic = [0u8; 16];
    magic.copy_from_slice(&input[idx..idx + 16]);
    idx += 16;

    if idx + 2 > input.len() {
        return Err("missing pong length".to_string());
    }

    let pong_len = u16::from_be_bytes([input[idx], input[idx + 1]]) as usize;
    idx += 2;

    if idx + pong_len > input.len() {
        return Err("invalid pong length".to_string());
    }

    let pong_data = std::str::from_utf8(&input[idx..idx + pong_len])
        .map_err(|e| format!("invalid utf8 pong payload: {e}"))?;

    Ok(UnconnectedPing {
        ping_time,
        id,
        magic,
        pong: read_pong(pong_data),
    })
}

fn read_pong(raw: &str) -> PongData {
    let parts: Vec<&str> = raw.split(';').collect();

    PongData {
        edition: part(&parts, 0),
        motd: part(&parts, 1),
        protocol_version: part(&parts, 2),
        version: part(&parts, 3),
        players: part(&parts, 4),
        max_players: part(&parts, 5),
        server_id: part(&parts, 6),
        sub_motd: part(&parts, 7),
        game_type: part(&parts, 8),
        nintendo_limited: part(&parts, 9),
        port4: part(&parts, 10),
        port6: part(&parts, 11),
    }
}

fn write_pong(pong: &PongData) -> String {
    let fields = [
        pong.edition.as_str(),
        pong.motd.as_str(),
        pong.protocol_version.as_str(),
        pong.version.as_str(),
        pong.players.as_str(),
        pong.max_players.as_str(),
        pong.server_id.as_str(),
        pong.sub_motd.as_str(),
        pong.game_type.as_str(),
        pong.nintendo_limited.as_str(),
        pong.port4.as_str(),
        pong.port6.as_str(),
    ];

    let joined = fields.join(";");
    let trimmed = joined.trim_end_matches(';');

    format!("{trimmed};")
}

fn part(parts: &[&str], idx: usize) -> String {
    parts.get(idx).copied().unwrap_or_default().to_string()
}

impl fmt::Display for PongData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{};{};{};{};{};{};{};{};{};{};{};{};",
            self.edition,
            self.motd,
            self.protocol_version,
            self.version,
            self.players,
            self.max_players,
            self.server_id,
            self.sub_motd,
            self.game_type,
            self.nintendo_limited,
            self.port4,
            self.port6
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_read_round_trip() {
        let original = UnconnectedPing {
            ping_time: [1; 8],
            id: [2; 8],
            magic: [3; 16],
            pong: PongData {
                edition: "MCPE".to_string(),
                motd: "A".to_string(),
                protocol_version: "1".to_string(),
                version: "2".to_string(),
                players: "3".to_string(),
                max_players: "4".to_string(),
                server_id: "5".to_string(),
                sub_motd: "6".to_string(),
                game_type: "7".to_string(),
                nintendo_limited: "8".to_string(),
                port4: "19132".to_string(),
                port6: "19132".to_string(),
            },
        };

        let bytes = original.build();
        let parsed = read_unconnected_ping(&bytes).expect("parse should succeed");

        assert_eq!(parsed.ping_time, original.ping_time);
        assert_eq!(parsed.id, original.id);
        assert_eq!(parsed.magic, original.magic);
        assert_eq!(parsed.pong.port4, "19132");
    }
}
