mod support;

use sdmmc_async::testing::RamCard;
use sdmmc_async::{Error, File, Volume};
use support::{block_on, exfat_image_writable, read_all, ExFileSpec};

type Vol = Volume<RamCard>;

fn spec<'a>(name: &'a str, data: &'a [u8], no_fat_chain: bool) -> ExFileSpec<'a> {
    ExFileSpec {
        name,
        data,
        no_fat_chain,
    }
}

fn mount(image: Vec<u8>) -> Vol {
    block_on(Volume::mount(RamCard::new(image))).unwrap()
}

/// Unmounts and mounts again, the way a power cycle would, keeping only what reached the card.
fn remount(mut volume: Vol) -> Vol {
    block_on(volume.sync()).unwrap();
    mount(volume.into_device().into_image())
}

fn open(volume: &mut Vol, path: &str) -> File {
    block_on(volume.open_file(path)).unwrap()
}

fn pattern(length: usize, seed: u8) -> Vec<u8> {
    (0..length)
        .map(|index| (index as u8).wrapping_mul(13).wrapping_add(seed))
        .collect()
}

fn base() -> Vol {
    mount(exfat_image_writable(&[
        spec("A.TXT", b"0123456789", false),
        spec("EMPTY.TXT", b"", false),
        spec("BIG.BIN", &pattern(1500, 1), false),
        spec("CONTIG.BIN", &pattern(1200, 6), true),
    ]))
}

#[test]
fn overwriting_inside_a_file_changes_only_those_bytes() {
    let mut volume = base();
    let mut file = open(&mut volume, "A.TXT");
    block_on(volume.seek(&mut file, 3)).unwrap();
    assert_eq!(block_on(volume.write_file(&mut file, b"abc")).unwrap(), 3);
    block_on(volume.flush_file(&mut file)).unwrap();
    let mut volume = remount(volume);
    let mut file = open(&mut volume, "A.TXT");
    assert_eq!(read_all(&mut volume, &mut file), b"012abc6789");
}

#[test]
fn appending_grows_the_file_across_cluster_boundaries() {
    let mut volume = base();
    let mut file = open(&mut volume, "A.TXT");
    block_on(volume.seek(&mut file, 10)).unwrap();
    let extra = pattern(2000, 7);
    assert_eq!(
        block_on(volume.write_file(&mut file, &extra)).unwrap(),
        extra.len()
    );
    assert_eq!(file.size(), 2010);
    block_on(volume.flush_file(&mut file)).unwrap();
    let mut volume = remount(volume);
    let mut file = open(&mut volume, "A.TXT");
    let mut expected = b"0123456789".to_vec();
    expected.extend(&extra);
    assert_eq!(read_all(&mut volume, &mut file), expected);
}

#[test]
fn a_write_that_is_not_flushed_does_not_change_the_size_seen_after_a_remount() {
    let mut volume = base();
    let mut file = open(&mut volume, "A.TXT");
    block_on(volume.seek(&mut file, 10)).unwrap();
    block_on(volume.write_file(&mut file, b"more")).unwrap();
    let mut volume = remount(volume);
    let mut file = open(&mut volume, "A.TXT");
    assert_eq!(read_all(&mut volume, &mut file), b"0123456789");
}

#[test]
fn a_write_into_an_empty_file_allocates_its_first_cluster() {
    let mut volume = base();
    let mut file = open(&mut volume, "EMPTY.TXT");
    assert_eq!(block_on(volume.write_file(&mut file, b"hello")).unwrap(), 5);
    block_on(volume.flush_file(&mut file)).unwrap();
    let mut volume = remount(volume);
    let mut file = open(&mut volume, "EMPTY.TXT");
    assert_eq!(read_all(&mut volume, &mut file), b"hello");
}

#[test]
fn appending_after_a_seek_to_the_end_and_after_reading_works() {
    let mut volume = base();
    let mut file = open(&mut volume, "BIG.BIN");
    let mut original = read_all(&mut volume, &mut file);
    // The position is at the end after reading it all.
    let more = pattern(700, 9);
    block_on(volume.write_file(&mut file, &more)).unwrap();
    original.extend(&more);
    block_on(volume.seek(&mut file, 0)).unwrap();
    let end = file.size();
    block_on(volume.seek(&mut file, end)).unwrap();
    let again = pattern(10, 3);
    block_on(volume.write_file(&mut file, &again)).unwrap();
    original.extend(&again);
    block_on(volume.flush_file(&mut file)).unwrap();
    let mut volume = remount(volume);
    let mut file = open(&mut volume, "BIG.BIN");
    assert_eq!(read_all(&mut volume, &mut file), original);
}

#[test]
fn many_small_writes_equal_one_big_one() {
    let mut volume = base();
    let mut file = open(&mut volume, "EMPTY.TXT");
    let data = pattern(3000, 5);
    for chunk in data.chunks(37) {
        assert_eq!(
            block_on(volume.write_file(&mut file, chunk)).unwrap(),
            chunk.len()
        );
    }
    block_on(volume.flush_file(&mut file)).unwrap();
    let mut volume = remount(volume);
    let mut file = open(&mut volume, "EMPTY.TXT");
    assert_eq!(read_all(&mut volume, &mut file), data);
}

#[test]
fn shortening_a_file_frees_its_clusters_for_reuse() {
    let mut volume = base();
    let mut file = open(&mut volume, "BIG.BIN");
    block_on(volume.set_len(&mut file, 600)).unwrap();
    block_on(volume.flush_file(&mut file)).unwrap();
    let mut volume = remount(volume);
    let mut file = open(&mut volume, "BIG.BIN");
    assert_eq!(read_all(&mut volume, &mut file), pattern(1500, 1)[..600]);

    // The freed clusters can hold another file's data.
    let mut other = open(&mut volume, "EMPTY.TXT");
    let data = pattern(900, 2);
    block_on(volume.write_file(&mut other, &data)).unwrap();
    block_on(volume.flush_file(&mut other)).unwrap();
    let mut volume = remount(volume);
    let mut other = open(&mut volume, "EMPTY.TXT");
    assert_eq!(read_all(&mut volume, &mut other), data);
    let mut file = open(&mut volume, "BIG.BIN");
    assert_eq!(read_all(&mut volume, &mut file), pattern(1500, 1)[..600]);
}

#[test]
fn truncating_to_zero_then_writing_starts_the_file_over() {
    let mut volume = base();
    let mut file = open(&mut volume, "BIG.BIN");
    block_on(volume.set_len(&mut file, 0)).unwrap();
    assert_eq!(file.size(), 0);
    assert_eq!(file.position(), 0);
    block_on(volume.write_file(&mut file, b"fresh")).unwrap();
    block_on(volume.flush_file(&mut file)).unwrap();
    let mut volume = remount(volume);
    let mut file = open(&mut volume, "BIG.BIN");
    assert_eq!(read_all(&mut volume, &mut file), b"fresh");
}

#[test]
fn lengthening_a_file_fills_the_gap_with_zeros() {
    let mut volume = base();
    let mut file = open(&mut volume, "A.TXT");
    block_on(volume.set_len(&mut file, 1300)).unwrap();
    block_on(volume.flush_file(&mut file)).unwrap();
    let mut volume = remount(volume);
    let mut file = open(&mut volume, "A.TXT");
    let content = read_all(&mut volume, &mut file);
    assert_eq!(content.len(), 1300);
    assert_eq!(&content[..10], b"0123456789");
    assert!(content[10..].iter().all(|byte| *byte == 0));
}

#[test]
fn filling_the_volume_reports_no_space_and_keeps_what_fit() {
    let mut volume = base();
    let mut file = open(&mut volume, "EMPTY.TXT");
    let chunk = [0xa5u8; 512];
    let mut written = 0usize;
    loop {
        match block_on(volume.write_file(&mut file, &chunk)) {
            Ok(count) => {
                written += count;
                if count < chunk.len() {
                    break;
                }
            }
            Err(Error::NoSpace) => break,
            Err(other) => panic!("{other:?}"),
        }
    }
    assert!(written > 100 * 512, "{written}");
    assert!(matches!(
        block_on(volume.write_file(&mut file, &chunk)),
        Err(Error::NoSpace)
    ));
    block_on(volume.flush_file(&mut file)).unwrap();
    let mut volume = remount(volume);
    let mut file = open(&mut volume, "EMPTY.TXT");
    assert_eq!(read_all(&mut volume, &mut file).len(), written);
}

#[test]
fn a_contiguous_file_stays_contiguous_while_the_next_cluster_is_free() {
    let mut volume = base();
    let mut file = open(&mut volume, "CONTIG.BIN");
    let mut expected = pattern(1200, 6);
    // CONTIG.BIN is the last file on the volume, so the clusters after it are free.
    let more = pattern(1000, 8);
    let end = file.size();
    block_on(volume.seek(&mut file, end)).unwrap();
    block_on(volume.write_file(&mut file, &more)).unwrap();
    expected.extend(&more);
    block_on(volume.flush_file(&mut file)).unwrap();
    let mut volume = remount(volume);
    let mut file = open(&mut volume, "CONTIG.BIN");
    assert_eq!(read_all(&mut volume, &mut file), expected);
}

#[test]
fn a_contiguous_file_that_is_blocked_becomes_a_chain() {
    let mut volume = base();
    // Fill the clusters after CONTIG.BIN so it cannot grow in place.
    let mut other = block_on(volume.create_file("OTHER.BIN")).unwrap();
    block_on(volume.write_file(&mut other, &pattern(3000, 2))).unwrap();
    block_on(volume.flush_file(&mut other)).unwrap();
    let mut file = open(&mut volume, "CONTIG.BIN");
    let mut expected = pattern(1200, 6);
    let end = file.size();
    block_on(volume.seek(&mut file, end)).unwrap();
    let more = pattern(2000, 9);
    block_on(volume.write_file(&mut file, &more)).unwrap();
    expected.extend(&more);
    block_on(volume.flush_file(&mut file)).unwrap();
    let mut volume = remount(volume);
    let mut file = open(&mut volume, "CONTIG.BIN");
    assert_eq!(read_all(&mut volume, &mut file), expected);
    let mut other = open(&mut volume, "OTHER.BIN");
    assert_eq!(read_all(&mut volume, &mut other), pattern(3000, 2));
}

#[test]
fn shortening_a_contiguous_file_frees_the_tail() {
    let mut volume = base();
    let mut file = open(&mut volume, "CONTIG.BIN");
    block_on(volume.set_len(&mut file, 600)).unwrap();
    block_on(volume.flush_file(&mut file)).unwrap();
    let mut volume = remount(volume);
    let mut file = open(&mut volume, "CONTIG.BIN");
    assert_eq!(read_all(&mut volume, &mut file), pattern(1200, 6)[..600]);
    // The freed cluster is reused.
    let mut other = block_on(volume.create_file("N.BIN")).unwrap();
    block_on(volume.write_file(&mut other, &pattern(600, 1))).unwrap();
    block_on(volume.flush_file(&mut other)).unwrap();
    let mut volume = remount(volume);
    let mut file = open(&mut volume, "CONTIG.BIN");
    assert_eq!(read_all(&mut volume, &mut file), pattern(1200, 6)[..600]);
}
