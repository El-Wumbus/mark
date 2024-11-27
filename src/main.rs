use std::io::{BufReader, BufWriter, Read as _, Write as _};
use std::str::FromStr;
use std::process::exit;

// TODO: parse .gitignore files and use them to ignore files by default
//       https://git-scm.com/docs/gitignore
//
// TODO: Input/Output on stdin/stdout by default
// TODO: Unarchival/unpacking

#[derive(Default)]
struct Opts {
    /// Input file -- stdin if omitted.
    input: Option<String>,
    /// Output file -- stdout if omitted.
    output: Option<String>,
    /// Whether to use dotfiles
    include_dotfiles: bool,
    /// Which compression method to use
    compression_method: DataCompression,
}

fn parse_flags(args: Vec<String>) -> (Opts, Vec<String>) {
    let mut opts = Opts::default();
    let mut positionals = vec![];
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        if !arg.starts_with('-') {
            positionals.push(arg);
            continue;
        }

        match arg.as_str() {
            "-" => {
                positionals.extend_from_slice(&args.collect::<Vec<_>>());
                break;
            }
            "include-dotfiles" => {
                opts.include_dotfiles = true;
            }
            "compress" => {
                let Some(compression_method) = args.next().map(|x|x.to_lowercase()).and_then(|x|DataCompression::from_str(&x).ok()) else {
                    eprintln!("I expected a valid compression type after -compress");
                    exit(1);
                };
                opts.compression_method = compression_method;
            }
            unrecognized_flag => {
                eprintln!(
                    "Unrecognized flag \"-{unrecognized_flag}\", treating it like a positional."
                );
                positionals.push(arg);
            }
        }
    }
    (opts, positionals)
}

fn main() {
    let (opts, positionals) = parse_flags(std::env::args().skip(1).collect::<Vec<_>>());
    let mut positionals = positionals.into_iter();
    let Some(subcommand) = positionals.next() else {
        eprintln!("Expected a subcommand!");
        exit(1);
    };

    let input: &dyn std::io::BufRead = match opts.input.as_deref() {
        Some(input) => &BufReader::new(std::fs::File::open(input).unwrap()),
        None => &BufReader::new(std::io::stdin().lock()),
    };
    let output: &dyn std::io::Write = match opts.output.as_deref() {
        Some(output) => &BufWriter::new(std::fs::File::create(output).unwrap()),
        None => &BufWriter::new(std::io::stdout().lock()),
    };

    match subcommand.as_str() {
        "archive" => archive(opts, &positionals.collect::<Vec<_>>()),
        "read-archive" => {
            for arg in positionals {
                read_archive(&arg);
            }
        }
        "unarchive" => {
            for arg in positionals {
            }
        }
        _ => {
            eprintln!("Invalid subcommand!");
            exit(1);
        }
    }
}

fn read_archive(archive: &str) {
    let mut files = vec![];

    let mut archive = BufReader::new(std::fs::File::open(archive).unwrap());

    let mut header_buf = [0u8; ArchiveHeader::SIZE];
    archive.read_exact(&mut header_buf).unwrap();
    let header = ArchiveHeader::from_le_bytes(header_buf);

    for _ in 0..header.file_count {
        let file = FileHeaderRepr::read(&mut archive, false).unwrap();
        // Skip past the file data
        
        files.push(file);
    }

    println!(
        "Format version: {}; File count: {}",
        header.version, header.file_count
    );
    for file in files.iter() {
        println!(
            "{} :: {{ mode = {:o}; uncompressed_len = {}; compressed_len = {}; compression_method = {:?} }}",
            file.name, file.mode, file.data_uncompressed_len, file.data_len,  file.data_compression,
        );
    }
}

fn unarchive() {
    todo!()
}

fn archive(opts: Opts, args: &[String]) {
    use std::os::unix::fs::MetadataExt;
    if args.is_empty() {
        eprintln!("Expected one or more files or directories to archive!");
        exit(1);
    }

    let mut files = vec![];

    for a in args {
        let path = std::path::Path::new(a.as_str());
        if !opts.include_dotfiles
            && path
                .file_name()
                .is_some_and(|n| n.as_encoded_bytes()[0] == b'.')
        {
            continue;
        }
        let parent = path.parent();
        walk(path, &mut |is_dir, path| {
            if !opts.include_dotfiles
                && path
                    .file_name()
                    .is_some_and(|n| n.as_encoded_bytes()[0] == b'.')
            {
                return Ok(false);
            }
            if !is_dir {
                let name = if let Some(parent) = parent {
                    path.strip_prefix(parent).unwrap()
                } else {
                    path
                };
                let name = name.to_str().unwrap().to_string();
                files.push((name, std::fs::canonicalize(path)?));
            }
            Ok(true)
        })
        .unwrap();
    }
    files.sort_by(|l, r| l.1.cmp(&r.1));
    files.dedup_by(|l, r| l.1 == r.1);

    let mut archive = BufWriter::new(std::fs::File::create("test.mark").unwrap());
    archive
        .write_all(
            &ArchiveHeader {
                version: 0,
                file_count: files.len() as u32,
            }
            .to_le_bytes(),
        )
        .unwrap();

    for (name, path) in files {
        let mut buf = vec![];
        let metadata = std::fs::metadata(&path).unwrap();
        let uncompressed_size = metadata.len();
        let compressed_size = match opts.compression_method {
            DataCompression::None => {
                std::fs::File::open(&path).unwrap().read_to_end(&mut buf).unwrap()
            }
            DataCompression::Brotli => {
                brotli::enc::reader::CompressorReader::with_params(
                    std::fs::File::open(&path).unwrap(), 4096, &BROTLI_ENC_PARAMS
                ).read_to_end(&mut buf).unwrap()
            }
        };

        let f = FileHeaderRepr::new(
            metadata.mode(),
            opts.compression_method,
            uncompressed_size as u64,
            compressed_size as u64,
            name,
            buf,
        );
        eprintln!("Writing: {} :: {{ mode = {:o}; compression = {:?}; uncompressed_len = {}; len = {} }}",
            f.name, f.mode, f.data_compression, f.data_uncompressed_len, f.data_len);
        f.write(&mut archive).unwrap();
    }
}

fn walk(
    p: impl AsRef<std::path::Path>,
    callback: &mut dyn FnMut(bool, &std::path::Path) -> std::io::Result<bool>,
) -> Result<(), std::io::Error> {
    let dir = p.as_ref();
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                if callback(true, &path)? {
                    walk(path, callback)?;
                }
            } else {
                callback(false, &path)?;
            }
        }
    } else {
        // We don't want to ignore the first item if it's a file
        callback(false, dir)?;
    }
    Ok(())
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArchiveHeader {
    version: u32,
    file_count: u32,
}

impl ArchiveHeader {
    const SIZE: usize = 8;
    #[inline]
    fn to_le_bytes(self) -> [u8; Self::SIZE] {
        let mut a = [0u8; Self::SIZE];
        a[0..4].copy_from_slice(&self.version.to_le_bytes());
        a[4..8].copy_from_slice(&self.file_count.to_le_bytes());
        a
    }

    #[inline]
    fn from_le_bytes(a: [u8; Self::SIZE]) -> Self {
        let mut v = [0u8; 4];
        v.copy_from_slice(&a[0..4]);
        let mut fc = [0u8; 4];
        fc.copy_from_slice(&a[4..8]);
        let version = u32::from_le_bytes(v);
        let file_count = u32::from_le_bytes(fc);

        Self {
            version,
            file_count,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
enum DataCompression {
    None = 0,
    #[default]
    Brotli = 1,
}

    lazy_static::lazy_static! {
        pub static ref BROTLI_ENC_PARAMS: brotli::enc::BrotliEncoderParams = {
            
        brotli::enc::BrotliEncoderParams::default()
        };
    }
impl TryFrom<u8> for DataCompression {
    type Error = ();
    fn try_from(x: u8) -> Result<DataCompression, Self::Error> {
        match x {
            0 => Ok(DataCompression::None),
            1 => Ok(Self::Brotli),
            _ => Err(()),
        }
    }
}

impl std::str::FromStr for DataCompression {
    type Err = &'static str;

    // Required method
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "default" => Self::default(),
            "none" => Self::None,
            "brotli" => Self::Brotli,
            _=> return Err("unspported compression format")
        })
    }
}

/// FileHeader
///
/// Layout:
///
/// | Information            |   Size in bytes   |
/// | ---------------------- | ----------------- |
/// | mode:                  | 4                 |
/// | name_len:              | 3                 |
/// | data_compression:      | 1                 |
/// | data_uncompressed_len  | 8                 |
/// | data_len:              | 8                 |
/// | name:                  | name_len          |
/// | data:                  | data_len          |
#[derive(Debug,Default, Clone, Copy)]
struct FileHeader {
    /// The UNIX file permissions
    mode: u32,
    /// Data compression takes up the higher 8 bits, where the lower 24 are for name length.
    data_compression_and_name_len: u32,
    /// The size of the file data prior to compression, if any has been applied.
    data_uncompressed_len: u64,
    /// The size of the file data within the archive
    data_len: u64,
    // name: &'a [u8],
    // data: &'a [u8],
}

impl FileHeader {
    pub const SIZE: usize = 24;
    #[inline]
    fn new(
        mode: u32,
        name_len: u32,
        data_compression: DataCompression,
        data_uncompressed_len: u64,
        data_len: u64,
    ) -> Self {
        Self {
            mode,
            data_compression_and_name_len: data_compression as u32 | name_len << 8,
            data_uncompressed_len,
            data_len,
        }
    }

    fn read<R: std::io::Read>(reader: &mut R) -> std::io::Result<Self> {
        let mut s = Self::default();
        let mut file_header_buf = [0u8; Self::SIZE];
        reader.read_exact(&mut file_header_buf)?;

        s.mode = u32::from_le_bytes(file_header_buf[0..4].try_into().unwrap());
        s.data_compression_and_name_len = u32::from_le_bytes(file_header_buf[4..8].try_into().unwrap());
        s.data_uncompressed_len = u64::from_le_bytes(file_header_buf[8..16].try_into().unwrap());
        s.data_len = u64::from_le_bytes(file_header_buf[16..24].try_into().unwrap());
        Ok(s)
    }

    fn write<W: std::io::Write>(self, writer: &mut W) -> std::io::Result<()> {
        writer.write_all(&self.mode.to_le_bytes())?;
        writer.write_all(&self.data_compression_and_name_len.to_le_bytes())?;
        writer.write_all(&self.data_uncompressed_len.to_le_bytes())?;
        writer.write_all(&self.data_len.to_le_bytes())?;
        Ok(())
    }

    #[inline]
    fn data_compression(&self) -> DataCompression {
        DataCompression::try_from(self.data_compression_and_name_len as u8).unwrap()
    }

    #[inline]
    fn name_len(&self) -> u32 {
        self.data_compression_and_name_len >> 8
    }

}

#[derive(Debug, Clone)]
struct FileHeaderRepr {
    mode: u32,
    data_compression: DataCompression,
    data_uncompressed_len: u64,
    data_len: u64,

    name: String,
    data: Vec<u8>,
}

impl FileHeaderRepr {
    fn new(mode: u32, data_compression: DataCompression, data_uncompressed_len: u64, data_len: u64, name: String, data: Vec<u8>) -> Self {
        Self {mode, data_compression, data_uncompressed_len, data_len, name, data}
    }
    fn read<R: std::io::Read + std::io::Seek>(reader: &mut R, skip_data: bool) -> std::io::Result<Self> {
        let header = FileHeader::read(reader)?;
        let mut name = vec![0u8; header.name_len() as usize];
        reader.read_exact(&mut name)?;
        let name = String::from_utf8(name).unwrap();

        let data = if skip_data {
            reader.seek_relative(header.data_len as i64)?;
            vec![]
        } else {
            let mut data = vec![0u8; header.data_len as usize];
            reader.read_exact(&mut data)?;
            data
        };

        Ok(Self {
            mode: header.mode,
            data_compression: header.data_compression(),
            data_uncompressed_len: header.data_uncompressed_len,
            data_len: header.data_len,
            name,
            data,
        })
    }

    fn write<W: std::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        let header = FileHeader::new(
            self.mode,
            self.name.len() as u32,
            self.data_compression,
            self.data_uncompressed_len,
            self.data_len,
        );
        header.write(writer)?;
        writer.write_all(self.name.as_bytes())?;
        writer.write_all(&self.data)?;

        Ok(())
    }
}
