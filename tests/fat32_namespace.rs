mod support;

use sdmmc_async::testing::RamCard;
use sdmmc_async::{Error, Volume};
use support::{block_on, fat32_image, read_all};

type Vol = Volume<RamCard>;

fn fresh() -> Vol {
    block_on(Volume::mount(RamCard::new(fat32_image(false, &[])))).unwrap()
}

fn remount(mut volume: Vol) -> Vol {
    block_on(volume.sync()).unwrap();
    block_on(Volume::mount(RamCard::new(
        volume.into_device().into_image(),
    )))
    .unwrap()
}

fn names(volume: &mut Vol, path: &str) -> Vec<String> {
    let mut dir = block_on(volume.open_dir(path)).unwrap();
    let mut out = Vec::new();
    while let Some(entry) = block_on(volume.next_entry(&mut dir)).unwrap() {
        out.push(entry.name.as_str().to_string());
    }
    out.sort();
    out
}

fn put(volume: &mut Vol, path: &str, data: &[u8]) {
    let mut file = block_on(volume.create_file(path)).unwrap();
    assert_eq!(
        block_on(volume.write_file(&mut file, data)).unwrap(),
        data.len()
    );
    block_on(volume.flush_file(&mut file)).unwrap();
}

fn get(volume: &mut Vol, path: &str) -> Vec<u8> {
    let mut file = block_on(volume.open_file(path)).unwrap();
    read_all(volume, &mut file)
}

fn pattern(length: usize, seed: u8) -> Vec<u8> {
    (0..length)
        .map(|index| (index as u8).wrapping_mul(29).wrapping_add(seed))
        .collect()
}

#[test]
fn a_created_file_is_there_after_a_remount() {
    let mut volume = fresh();
    put(&mut volume, "HELLO.TXT", b"hi");
    let mut volume = remount(volume);
    assert_eq!(names(&mut volume, ""), ["HELLO.TXT"]);
    assert_eq!(get(&mut volume, "hello.txt"), b"hi");
}

#[test]
fn lower_case_short_names_keep_their_case_without_long_name_entries() {
    let mut volume = fresh();
    put(&mut volume, "readme.txt", b"x");
    let mut volume = remount(volume);
    assert_eq!(names(&mut volume, ""), ["readme.txt"]);
}

#[test]
fn long_and_mixed_case_names_round_trip() {
    let mut volume = fresh();
    let all = [
        "Mixed.Case",
        "a-very-long-file-name.gcode",
        "with space.txt",
        "ünïcode-näme.txt",
        "many.dots.in.name.txt",
        ".hidden",
        "UPPER.TXT",
    ];
    for (index, name) in all.iter().enumerate() {
        put(&mut volume, name, &pattern(40 + index, index as u8));
    }
    let mut volume = remount(volume);
    let mut expected: Vec<String> = all.iter().map(|name| name.to_string()).collect();
    expected.sort();
    assert_eq!(names(&mut volume, ""), expected);
    for (index, name) in all.iter().enumerate() {
        assert_eq!(
            get(&mut volume, name),
            pattern(40 + index, index as u8),
            "{name}"
        );
    }
}

#[test]
fn names_that_shorten_to_the_same_8_3_name_get_different_aliases() {
    let mut volume = fresh();
    for index in 0..12 {
        put(
            &mut volume,
            &format!("long-file-name-{index}.txt"),
            &[index as u8],
        );
    }
    let mut volume = remount(volume);
    for index in 0..12 {
        assert_eq!(
            get(&mut volume, &format!("long-file-name-{index}.txt")),
            [index as u8]
        );
    }
    // The 8.3 aliases are reachable as well and distinct.
    assert_eq!(get(&mut volume, "LONG-F~1.TXT").len(), 1);
    assert_eq!(get(&mut volume, "LONG-F~2.TXT").len(), 1);
}

#[test]
fn a_name_is_taken_regardless_of_case_or_alias() {
    let mut volume = fresh();
    put(&mut volume, "Report-2024.txt", b"a");
    for clash in ["report-2024.TXT", "REPORT~1.TXT"] {
        assert!(matches!(
            block_on(volume.create_file(clash)),
            Err(Error::AlreadyExists)
        ));
    }
}

#[test]
fn invalid_names_are_refused() {
    let mut volume = fresh();
    for bad in [
        "",
        "a:b",
        "a*b",
        "a?b",
        "a\"b",
        "a<b",
        "a|b",
        "a\\b",
        "trailing.",
        "trailing ",
        "ctl\u{1}x",
    ] {
        assert!(
            matches!(block_on(volume.create_file(bad)), Err(Error::InvalidName)),
            "{bad:?}"
        );
    }
    let long = "x".repeat(256);
    assert!(matches!(
        block_on(volume.create_file(&long)),
        Err(Error::InvalidName)
    ));
    let longest = "y".repeat(255);
    put(&mut volume, &longest, b"ok");
    let mut volume = remount(volume);
    assert_eq!(get(&mut volume, &longest), b"ok");
}

#[test]
fn directories_nest_and_hold_files() {
    let mut volume = fresh();
    block_on(volume.create_dir("GCODE")).unwrap();
    block_on(volume.create_dir("GCODE/old stuff")).unwrap();
    put(&mut volume, "GCODE/benchy.gco", &pattern(1500, 3));
    put(&mut volume, "GCODE/old stuff/a.gco", b"old");
    let mut volume = remount(volume);
    assert_eq!(names(&mut volume, ""), ["GCODE"]);
    assert_eq!(names(&mut volume, "GCODE"), ["benchy.gco", "old stuff"]);
    assert_eq!(names(&mut volume, "GCODE/old stuff"), ["a.gco"]);
    assert_eq!(get(&mut volume, "gcode/BENCHY.GCO"), pattern(1500, 3));
    assert!(matches!(
        block_on(volume.create_file("MISSING/x.txt")),
        Err(Error::NotFound)
    ));
    assert!(matches!(
        block_on(volume.create_file("GCODE/benchy.gco/x")),
        Err(Error::NotADirectory)
    ));
}

#[test]
fn a_directory_that_fills_up_grows() {
    let mut volume = fresh();
    block_on(volume.create_dir("D")).unwrap();
    // One cluster (one sector here) holds 16 entries; long names need more than one each.
    for index in 0..60 {
        put(
            &mut volume,
            &format!("D/file-number-{index:03}.txt"),
            &[index as u8],
        );
    }
    let mut volume = remount(volume);
    assert_eq!(names(&mut volume, "D").len(), 60);
    for index in 0..60 {
        assert_eq!(
            get(&mut volume, &format!("D/file-number-{index:03}.txt")),
            [index as u8]
        );
    }
}

#[test]
fn removing_a_file_frees_its_space_and_its_name() {
    let mut volume = fresh();
    put(&mut volume, "BIG.BIN", &pattern(100 * 512, 1));
    block_on(volume.remove_file("big.bin")).unwrap();
    assert!(matches!(
        block_on(volume.open_file("BIG.BIN")),
        Err(Error::NotFound)
    ));
    // The space is free again: a file as big can be written twice over.
    put(&mut volume, "A.BIN", &pattern(150 * 512, 2));
    block_on(volume.remove_file("A.BIN")).unwrap();
    put(&mut volume, "B.BIN", &pattern(150 * 512, 3));
    put(&mut volume, "BIG.BIN", b"again");
    let mut volume = remount(volume);
    assert_eq!(names(&mut volume, ""), ["B.BIN", "BIG.BIN"]);
    assert_eq!(get(&mut volume, "B.BIN"), pattern(150 * 512, 3));
}

#[test]
fn removing_a_file_with_a_long_name_removes_all_its_entries() {
    let mut volume = fresh();
    put(&mut volume, "a-very-long-file-name.gcode", b"x");
    put(&mut volume, "keep.txt", b"k");
    block_on(volume.remove_file("a-very-long-file-name.gcode")).unwrap();
    let mut volume = remount(volume);
    assert_eq!(names(&mut volume, ""), ["keep.txt"]);
    // The slots are reusable.
    put(&mut volume, "another-long-file-name.gcode", b"y");
    let mut volume = remount(volume);
    assert_eq!(
        names(&mut volume, ""),
        ["another-long-file-name.gcode", "keep.txt"]
    );
}

#[test]
fn only_empty_directories_can_be_removed() {
    let mut volume = fresh();
    block_on(volume.create_dir("D")).unwrap();
    put(&mut volume, "D/f.txt", b"x");
    assert!(matches!(
        block_on(volume.remove_dir("D")),
        Err(Error::NotEmpty)
    ));
    assert!(matches!(
        block_on(volume.remove_file("D")),
        Err(Error::NotAFile)
    ));
    block_on(volume.remove_file("D/f.txt")).unwrap();
    block_on(volume.remove_dir("D")).unwrap();
    assert!(matches!(
        block_on(volume.remove_dir("D")),
        Err(Error::NotFound)
    ));
    let mut volume = remount(volume);
    assert!(names(&mut volume, "").is_empty());
}

#[test]
fn renaming_in_the_same_directory_keeps_the_data() {
    let mut volume = fresh();
    put(&mut volume, "old.txt", b"data");
    block_on(volume.rename("old.txt", "a-longer-new-name.txt")).unwrap();
    let mut volume = remount(volume);
    assert_eq!(names(&mut volume, ""), ["a-longer-new-name.txt"]);
    assert_eq!(get(&mut volume, "a-longer-new-name.txt"), b"data");
}

#[test]
fn renaming_can_only_change_the_case() {
    let mut volume = fresh();
    put(&mut volume, "name.txt", b"data");
    block_on(volume.rename("name.txt", "Name.TXT")).unwrap();
    let mut volume = remount(volume);
    assert_eq!(names(&mut volume, ""), ["Name.TXT"]);
}

#[test]
fn renaming_onto_an_existing_name_is_refused() {
    let mut volume = fresh();
    put(&mut volume, "a.txt", b"a");
    put(&mut volume, "b.txt", b"b");
    assert!(matches!(
        block_on(volume.rename("a.txt", "B.TXT")),
        Err(Error::AlreadyExists)
    ));
    assert_eq!(get(&mut volume, "a.txt"), b"a");
    assert_eq!(get(&mut volume, "b.txt"), b"b");
}

#[test]
fn moving_a_file_between_directories_works() {
    let mut volume = fresh();
    block_on(volume.create_dir("ONE")).unwrap();
    block_on(volume.create_dir("TWO")).unwrap();
    put(&mut volume, "ONE/f.bin", &pattern(900, 4));
    block_on(volume.rename("ONE/f.bin", "TWO/moved.bin")).unwrap();
    let mut volume = remount(volume);
    assert!(names(&mut volume, "ONE").is_empty());
    assert_eq!(get(&mut volume, "TWO/moved.bin"), pattern(900, 4));
}

#[test]
fn moving_a_directory_fixes_its_parent_link_and_refuses_to_move_into_itself() {
    let mut volume = fresh();
    block_on(volume.create_dir("A")).unwrap();
    block_on(volume.create_dir("A/B")).unwrap();
    block_on(volume.create_dir("C")).unwrap();
    put(&mut volume, "A/B/f.txt", b"deep");
    assert!(matches!(
        block_on(volume.rename("A", "A/B/A")),
        Err(Error::InvalidOperation)
    ));
    block_on(volume.rename("A/B", "C/B")).unwrap();
    let mut volume = remount(volume);
    assert!(names(&mut volume, "A").is_empty());
    assert_eq!(get(&mut volume, "C/B/f.txt"), b"deep");
    // `..` of the moved directory points at its new parent: the directory's second entry.
    let mut dir = block_on(volume.open_dir("C/B")).unwrap();
    assert_eq!(
        block_on(volume.next_entry(&mut dir))
            .unwrap()
            .unwrap()
            .name
            .as_str(),
        "f.txt"
    );
}

#[test]
fn a_file_created_then_written_to_later_through_open_file_keeps_its_name() {
    let mut volume = fresh();
    block_on(volume.create_file("later.txt")).unwrap();
    let mut volume = remount(volume);
    let mut file = block_on(volume.open_file("later.txt")).unwrap();
    block_on(volume.write_file(&mut file, b"now")).unwrap();
    block_on(volume.flush_file(&mut file)).unwrap();
    let mut volume = remount(volume);
    assert_eq!(get(&mut volume, "later.txt"), b"now");
}
