//! Read only the schema/migration b-trees, including committed WAL pages.
//! Opening the original with SQLite could change SHM; immutable mode misses WAL.
use anyhow::{ensure, Context, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

struct Pages {
    main: File,
    wal: Option<File>,
    offsets: BTreeMap<u32, u64>,
    size: usize,
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().unwrap())
}
fn checksum(bytes: &[u8], little: bool, mut state: [u32; 2]) -> [u32; 2] {
    for pair in bytes.chunks_exact(8) {
        let word = |b: &[u8]| {
            if little {
                u32::from_le_bytes(b.try_into().unwrap())
            } else {
                be32(b)
            }
        };
        state[0] = state[0]
            .wrapping_add(word(&pair[..4]))
            .wrapping_add(state[1]);
        state[1] = state[1]
            .wrapping_add(word(&pair[4..]))
            .wrapping_add(state[0]);
    }
    state
}
impl Pages {
    fn open(path: &Path) -> Result<Self> {
        let mut main = File::open(path)?;
        let mut header = [0; 100];
        main.read_exact(&mut header)
            .context("invalid SQLite header")?;
        ensure!(
            &header[..16] == b"SQLite format 3\0",
            "invalid SQLite header"
        );
        let raw = u16::from_be_bytes(header[16..18].try_into().unwrap());
        let size = if raw == 1 { 65536 } else { usize::from(raw) };
        ensure!(
            (512..=65536).contains(&size) && size.is_power_of_two(),
            "invalid SQLite page size"
        );
        let mut result = Self {
            main,
            wal: None,
            offsets: BTreeMap::new(),
            size,
        };
        let wal_path = super::startup::sibling(path, "-wal");
        if !wal_path.try_exists()? || std::fs::metadata(&wal_path)?.len() == 0 {
            return Ok(result);
        }
        let mut wal = File::open(wal_path)?;
        let mut header = [0; 32];
        wal.read_exact(&mut header)
            .context("invalid SQLite WAL header")?;
        let magic = be32(&header);
        ensure!(
            matches!(magic, 0x377f0682 | 0x377f0683)
                && be32(&header[4..]) == 3007000
                && be32(&header[8..]) as usize == size,
            "invalid SQLite WAL header"
        );
        let little = magic == 0x377f0682;
        let mut sum = checksum(&header[..24], little, [0; 2]);
        ensure!(
            sum == [be32(&header[24..]), be32(&header[28..])],
            "invalid SQLite WAL checksum"
        );
        let mut pending = BTreeMap::new();
        let mut frame = vec![0; 24 + size];
        let mut offset = 32u64;
        loop {
            match wal.read_exact(&mut frame) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
            if frame[8..16] != header[16..24] {
                break;
            }
            sum = checksum(&frame[..8], little, sum);
            sum = checksum(&frame[24..], little, sum);
            if sum != [be32(&frame[16..]), be32(&frame[20..])] {
                break;
            }
            let page = be32(&frame);
            ensure!(page != 0, "invalid SQLite WAL page");
            pending.insert(page, offset + 24);
            let committed_size = be32(&frame[4..]);
            if committed_size != 0 {
                result.offsets.append(&mut pending);
                result.offsets.retain(|page, _| *page <= committed_size);
            }
            offset += frame.len() as u64;
        }
        result.wal = Some(wal);
        Ok(result)
    }
    fn page(&mut self, number: u32) -> Result<Vec<u8>> {
        ensure!(number != 0, "invalid SQLite b-tree page");
        let mut bytes = vec![0; self.size];
        if let Some(offset) = self.offsets.get(&number) {
            let wal = self.wal.as_mut().unwrap();
            wal.seek(SeekFrom::Start(*offset))?;
            wal.read_exact(&mut bytes)?;
        } else {
            self.main
                .seek(SeekFrom::Start(u64::from(number - 1) * self.size as u64))?;
            self.main.read_exact(&mut bytes)?;
        }
        Ok(bytes)
    }
    fn visit(&mut self, root: u32, mut row: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let mut todo = vec![root];
        let mut seen = BTreeSet::new();
        while let Some(number) = todo.pop() {
            ensure!(
                seen.insert(number) && seen.len() <= 65536,
                "invalid or oversized SQLite migration b-tree"
            );
            let bytes = self.page(number)?;
            let start = if number == 1 { 100 } else { 0 };
            let kind = bytes[start];
            ensure!(
                matches!(kind, 5 | 13),
                "invalid SQLite migration table b-tree"
            );
            let count =
                u16::from_be_bytes(bytes[start + 3..start + 5].try_into().unwrap()) as usize;
            let pointer = start + if kind == 5 { 12 } else { 8 };
            ensure!(
                pointer + count * 2 <= bytes.len(),
                "invalid SQLite cell pointers"
            );
            if kind == 5 {
                todo.push(be32(&bytes[start + 8..]));
            }
            for cell in 0..count {
                let offset = pointer + cell * 2;
                let position =
                    u16::from_be_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize;
                let data = bytes
                    .get(position..)
                    .context("invalid SQLite cell offset")?;
                if kind == 5 {
                    ensure!(data.len() >= 4, "invalid SQLite interior cell");
                    todo.push(be32(data));
                } else {
                    let mut cursor = 0;
                    let _payload = varint(data, &mut cursor)?;
                    let _rowid = varint(data, &mut cursor)?;
                    // Only leading fields are needed. Schema SQL/checksums may
                    // overflow; their bytes are never interpreted or logged.
                    row(&data[cursor..])?;
                }
            }
        }
        Ok(())
    }
}
fn varint(bytes: &[u8], cursor: &mut usize) -> Result<u64> {
    let mut value = 0;
    for i in 0..9 {
        let byte = *bytes.get(*cursor).context("invalid SQLite record varint")?;
        *cursor += 1;
        if i == 8 {
            return Ok((value << 8) | u64::from(byte));
        }
        value = (value << 7) | u64::from(byte & 127);
        if byte & 128 == 0 {
            return Ok(value);
        }
    }
    unreachable!()
}
#[derive(Debug)]
enum Value<'a> {
    Integer(i64),
    Text(&'a [u8]),
    Other,
}
fn fields(bytes: &[u8], count: usize) -> Result<Vec<Value<'_>>> {
    let mut cursor = 0;
    let header = usize::try_from(varint(bytes, &mut cursor)?)?;
    ensure!(header <= bytes.len(), "invalid SQLite record header");
    let mut data = header;
    let mut result = Vec::new();
    for _ in 0..count {
        ensure!(cursor < header, "missing SQLite migration record field");
        let kind = varint(&bytes[..header], &mut cursor)?;
        let length = match kind {
            0 | 8 | 9 => 0,
            1..=4 => kind as usize,
            5 => 6,
            6 | 7 => 8,
            10 | 11 => anyhow::bail!("invalid SQLite serial type"),
            _ => usize::try_from((kind - 12) / 2)?,
        };
        let end = data
            .checked_add(length)
            .context("SQLite record length overflow")?;
        let field = bytes
            .get(data..end)
            .context("SQLite migration field exceeds local payload")?;
        result.push(match kind {
            1..=6 => {
                let mut value = if field[0] & 128 != 0 { -1i64 } else { 0 };
                for b in field {
                    value = (value << 8) | i64::from(*b);
                }
                Value::Integer(value)
            }
            8 => Value::Integer(0),
            9 => Value::Integer(1),
            k if k >= 13 && k % 2 == 1 => Value::Text(field),
            _ => Value::Other,
        });
        data = end;
    }
    Ok(result)
}

pub(super) fn check(path: &Path) -> Result<()> {
    let mut pages = Pages::open(path)?;
    let mut root = None;
    pages.visit(1, |bytes| {
        let fields = fields(bytes, 4)?;
        if matches!(fields[0], Value::Text(b"table"))
            && matches!(fields[1], Value::Text(b"_sqlx_migrations"))
        {
            if let Value::Integer(value) = fields[3] {
                root = Some(u32::try_from(value)?);
            }
        }
        Ok(())
    })?;
    let latest = super::source::SQLITE
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .map(|m| m.version)
        .max()
        .unwrap();
    if let Some(root) = root {
        pages.visit(root, |bytes| {
            let fields = fields(bytes, 1)?;
            if let Value::Integer(version) = fields[0] {
                ensure!(version <= latest, "SQLite source has migration {version}, this server knows up to {latest}: the database is from a newer release");
            } else { anyhow::bail!("invalid SQLite migration version"); }
            Ok(())
        })?;
    }
    Ok(())
}
