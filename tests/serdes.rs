use anyhow::Result;
use epserde::*;
use rand::rngs::SmallRng;
use rand::Rng;
use rand::RngCore;
use rand::SeedableRng;
use sux::prelude::*;

#[test]
fn test_serdes() -> Result<()> {
    let mut rng = SmallRng::seed_from_u64(0);

    let mut v = CompactArray::<Vec<u64>>::new(4, 200);
    for i in 0..200 {
        v.set(i, rng.gen_range(0..(1 << 4)));
    }

    let tmp_file = std::env::temp_dir().join("test_serdes_ef.bin");
    let mut file = std::io::BufWriter::new(std::fs::File::create(&tmp_file)?);
    v.serialize(&mut file)?;
    drop(file);

    let w = epserde::map::<CompactArray<Vec<u64>>>(&tmp_file, epserde::Flags::empty()).unwrap();

    for i in 0..200 {
        assert_eq!(v.get(i), w.get(i));
    }

    Ok(())
}
