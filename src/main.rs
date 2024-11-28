//! A simple archival format.
//!
//! For future editors:
//! Remember to always output debugging messages to stderr and not to stdout.

use byteorder::{LittleEndian, ReadBytesExt};
use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::exit;
use std::str::FromStr;
use std::time::{Duration, SystemTime};

// TODO: parse .gitignore files and use them to ignore files by default
//       https://git-scm.com/docs/gitignore

lazy_static::lazy_static! {
    pub static ref BROTLI_ENC_PARAMS: brotli::enc::BrotliEncoderParams = brotli::enc::BrotliEncoderParams::default();
}

#[derive(Default, Debug, Clone)]
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
            "--" => {
                positionals.extend_from_slice(&args.collect::<Vec<_>>());
                break;
            }
            "-i" | "-input" => {
                let Some(input) = args.next() else {
                    eprintln!("After -input, I expected a file path!");
                    exit(1);
                };
                opts.input = Some(input);
            }
            "-o" | "-output" => {
                let Some(output) = args.next() else {
                    eprintln!("After -output, I expected a file path!");
                    exit(1);
                };
                opts.output = Some(output);
            }
            "-include-dotfiles" => {
                opts.include_dotfiles = true;
            }
            "-compress" => {
                let Some(compression_method) = args
                    .next()
                    .map(|x| x.to_lowercase())
                    .and_then(|x| DataCompression::from_str(&x).ok())
                else {
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

    match subcommand.as_str() {
        "pack" => pack(opts, &positionals.collect::<Vec<_>>()),
        "unpack" => unpack(opts),
        "read" => read_archive(opts),
        _ => {
            eprintln!("Invalid subcommand!");
            exit(1);
        }
    }
}

fn pack(opts: Opts, args: &[String]) {
    use std::os::unix::fs::MetadataExt;

    let output: &mut dyn Write = match opts.output.as_deref() {
        Some(output) => &mut BufWriter::new(std::fs::File::create(output).unwrap()),
        None => &mut BufWriter::new(std::io::stdout().lock()),
    };

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

    ArchiveHeader {
        version: 0,
        file_count: files.len() as u32,
    }
    .write(output)
    .unwrap();
    for (name, path) in files {
        let mut buf = vec![];
        let metadata = std::fs::metadata(&path).unwrap();
        let uncompressed_size = metadata.len();
        let compressed_size = match opts.compression_method {
            DataCompression::None => std::fs::File::open(&path)
                .unwrap()
                .read_to_end(&mut buf)
                .unwrap(),
            DataCompression::Brotli => brotli::enc::reader::CompressorReader::with_params(
                std::fs::File::open(&path).unwrap(),
                8128,
                &BROTLI_ENC_PARAMS,
            )
            .read_to_end(&mut buf)
            .unwrap(),
        };
        let modified = metadata
            .modified()
            .unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let accessed = metadata
            .accessed()
            .unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let f = FileHeaderRepr::new(
            FileHeader {
                modified,
                accessed,
                mode: metadata.mode(),
                name_len: name.len() as u16,
                data_compression: opts.compression_method as u8,
                uncompressed_len: uncompressed_size as u64,
                len: compressed_size as u64,
            },
            name,
            buf,
        );
        eprintln!("Writing: {} :: {:?}", f.name, f.inner,);
        f.write(output).unwrap();
    }
}

fn read_archive(opts: Opts) {
    let input: &mut dyn Read = match opts.input.as_deref() {
        Some(input) => &mut BufReader::new(std::fs::File::open(input).unwrap()),
        None => &mut BufReader::new(std::io::stdin().lock()),
    };

    let mut files = vec![];

    let header = ArchiveHeader::read(input).unwrap();
    for _ in 0..header.file_count {
        let file = FileHeaderRepr::read(input, true).unwrap();
        files.push(file);
    }

    eprintln!(
        "Format version: {}; File count: {}",
        header.version, header.file_count
    );
    for file in files.iter() {
        eprintln!("{} :: {:?}", file.name, file.inner,);
    }
}

fn unpack(opts: Opts) {
    let input: &mut dyn Read = match opts.input.as_deref() {
        Some(input) => &mut BufReader::new(std::fs::File::open(input).unwrap()),
        None => &mut BufReader::new(std::io::stdin().lock()),
    };
    let output_dir = match opts.output {
        Some(o) => std::path::PathBuf::from(o),
        None => std::env::current_dir().unwrap(),
    };

    let header = ArchiveHeader::read(input).unwrap();
    for _ in 0..header.file_count {
        let file = FileHeaderRepr::read(input, false).unwrap();
        let file_path = output_dir.join(&file.name);
        if file_path.exists() {
            eprintln!("Not overwriting \"{}\"!", file_path.display());
            continue;
        }
        if let Some(parent) = file_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).unwrap();
            }
        }
        let mut output = std::fs::File::create(&file_path).unwrap();
        output
            .set_permissions(std::fs::Permissions::from_mode(file.inner.mode))
            .unwrap();

        eprintln!("Writing \"{}\" -> \"{}\"", file.name, file_path.display());
        let output = match DataCompression::try_from(file.inner.data_compression).unwrap() {
            DataCompression::None => {
                output.write_all(&file.data).unwrap();
                output
            }
            DataCompression::Brotli => {
                let mut x = brotli::DecompressorWriter::new(output, 8128);
                x.write_all(&file.data).unwrap();
                x.into_inner().unwrap()
            }
        };

        // set these after all the modifications are done so the changes stick
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(file.inner.modified);
        let accessed = SystemTime::UNIX_EPOCH + Duration::from_secs(file.inner.accessed);
        output
            .set_times(
                fs::FileTimes::new()
                    .set_accessed(accessed)
                    .set_modified(modified),
            )
            .unwrap();
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
    fn read(reader: &mut dyn Read) -> io::Result<Self> {
        let version = reader.read_u32::<LittleEndian>()?;
        let file_count = reader.read_u32::<LittleEndian>()?;
        Ok(Self {
            version,
            file_count,
        })
    }

    fn write(self, writer: &mut dyn Write) -> io::Result<()> {
        writer.write_all(&self.version.to_le_bytes())?;
        writer.write_all(&self.file_count.to_le_bytes())?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
enum DataCompression {
    None = 0,
    #[default]
    Brotli = 1,
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
            _ => return Err("unspported compression format"),
        })
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct FileHeader {
    modified: u64,
    accessed: u64,
    /// The UNIX file permissions
    mode: u32,
    name_len: u16,
    data_compression: u8,
    /// The size of the file data prior to compression, if any has been applied.
    uncompressed_len: u64,
    /// The size of the file data within the archive
    len: u64,
}

impl FileHeader {
    fn write(self, writer: &mut dyn Write) -> std::io::Result<()> {
        writer.write_all(&self.modified.to_le_bytes())?;
        writer.write_all(&self.accessed.to_le_bytes())?;
        writer.write_all(&self.mode.to_le_bytes())?;
        writer.write_all(&self.name_len.to_le_bytes())?;
        writer.write_all(&self.data_compression.to_le_bytes())?;
        writer.write_all(&self.uncompressed_len.to_le_bytes())?;
        writer.write_all(&self.len.to_le_bytes())?;
        Ok(())
    }

    fn read(reader: &mut dyn Read) -> std::io::Result<Self> {
        let modified = reader.read_u64::<LittleEndian>()?;
        let accessed = reader.read_u64::<LittleEndian>()?;
        let mode = reader.read_u32::<LittleEndian>()?;
        let name_len = reader.read_u16::<LittleEndian>()?;
        let data_compression = reader.read_u8()?;
        let uncompressed_len = reader.read_u64::<LittleEndian>()?;
        let len = reader.read_u64::<LittleEndian>()?;

        Ok(Self {
            modified,
            accessed,
            mode,
            data_compression,
            name_len,
            uncompressed_len,
            len,
        })
    }
}

#[derive(Debug, Clone)]
struct FileHeaderRepr {
    inner: FileHeader,
    name: String,
    data: Vec<u8>,
}

impl FileHeaderRepr {
    fn new(header: FileHeader, name: String, data: Vec<u8>) -> Self {
        Self {
            inner: header,
            name,
            data,
        }
    }

    fn read(reader: &mut dyn Read, skip_data: bool) -> std::io::Result<Self> {
        let header = FileHeader::read(reader)?;
        let name = {
            let mut name = vec![0u8; header.name_len as usize];
            reader.read_exact(&mut name)?;
            String::from_utf8(name).unwrap()
        };

        let data = if skip_data {
            io::copy(&mut reader.take(header.len as u64), &mut io::sink())?;
            vec![]
        } else {
            let mut data = vec![0u8; header.len as usize];
            reader.read_exact(&mut data)?;
            data
        };

        Ok(Self {
            inner: header,
            name,
            data,
        })
    }

    fn write(&self, writer: &mut dyn Write) -> std::io::Result<()> {
        self.inner.write(writer)?;
        writer.write_all(self.name.as_bytes())?;
        writer.write_all(&self.data)?;
        Ok(())
    }
}
