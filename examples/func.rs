use anyhow::Result;
use clap::{ArgGroup, Parser};
use dsi_progress_logger::ProgressLogger;
use epserde::prelude::*;
use epserde::Epserde;
use log::warn;
use rayon::prelude::*;
use std::io::{BufRead, BufReader};
use std::mem::{self};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;
use sux::prelude::{
    spooky::*, BitFieldSlice, BitFieldSliceAtomic, BitFieldSliceCore, CompactArray,
};
use sux::traits::convert_to::ConvertTo;
use Ordering::Relaxed;
pub trait Remap {
    fn remap(key: &Self, seed: u64) -> [u64; 2];
}

impl Remap for String {
    fn remap(key: &Self, seed: u64) -> [u64; 2] {
        let spooky = spooky_short(key.as_ref(), seed);
        [spooky[0], spooky[1]]
    }
}

impl Remap for str {
    fn remap(key: &Self, seed: u64) -> [u64; 2] {
        let spooky = spooky_short(key.as_ref(), seed);
        [spooky[0], spooky[1]]
    }
}

impl Remap for u64 {
    fn remap(key: &Self, seed: u64) -> [u64; 2] {
        let spooky = spooky_short(&key.to_ne_bytes(), seed);
        [spooky[0], spooky[1]]
    }
}

const PARAMS: [(usize, usize, f64); 15] = [
    (0, 1, 1.23),
    (13, 32, 1.22),
    (14, 32, 1.21),
    (15, 32, 1.20),
    (16, 64, 1.18),
    (17, 64, 1.16),
    (18, 64, 1.15),
    (19, 128, 1.14),
    (20, 128, 1.13),
    (21, 256, 1.12),
    (22, 512, 1.12),
    (23, 256, 1.11),
    (24, 512, 1.11),
    (25, 256, 1.11),
    (26, 512, 1.10),
];

#[derive(Debug, Default)]
struct EdgeList(usize);
impl EdgeList {
    const DEG_SHIFT: usize = usize::BITS as usize - 10;
    const EDGE_INDEX_MASK: usize = (1_usize << EdgeList::DEG_SHIFT) - 1;
    const DEG: usize = 1_usize << EdgeList::DEG_SHIFT;

    #[inline(always)]
    fn add(&mut self, edge: usize) {
        self.0 += EdgeList::DEG | edge;
    }

    #[inline(always)]
    fn remove(&mut self, edge: usize) {
        self.0 -= EdgeList::DEG | edge;
    }

    #[inline(always)]
    fn degree(&self) -> usize {
        self.0 >> EdgeList::DEG_SHIFT
    }

    #[inline(always)]
    fn edge_index(&self) -> usize {
        self.0 & EdgeList::EDGE_INDEX_MASK
    }

    #[inline(always)]
    fn dec(&mut self) {
        self.0 -= EdgeList::DEG;
    }
}

#[derive(Epserde, Debug, Default)]
pub struct Function<T: Remap, S: BitFieldSlice = CompactArray<Vec<usize>>> {
    seed: u64,
    log2_l: u32,
    high_bits: u32,
    chunk_mask: u32,
    num_keys: usize,
    segment_size: usize,
    values: S,
    _phantom: std::marker::PhantomData<T>,
}

impl<T: Remap, S: BitFieldSlice> Function<T, S> {
    #[inline(always)]
    #[must_use]
    fn chunk(sig: &[u64; 2], high_bits: u32, chunk_mask: u32) -> usize {
        (sig[0].rotate_left(high_bits) & chunk_mask as u64) as usize
    }

    #[inline(always)]
    #[must_use]
    fn edge(sig: &[u64; 2], log2_l: u32, segment_size: usize) -> [usize; 3] {
        let first_segment = (sig[0] >> 32 & ((1 << log2_l) - 1)) as usize;
        let start = first_segment * segment_size;
        [
            (((sig[0] & 0xFFFFFFFF) * segment_size as u64) >> 32) as usize + start,
            (((sig[1] & 0xFFFFFFFF) * segment_size as u64) >> 32) as usize + start + segment_size,
            (((sig[1] >> 32) * segment_size as u64) >> 32) as usize + start + 2 * segment_size,
        ]
    }

    pub fn get_by_sig(&self, sig: &[u64; 2]) -> usize {
        let edge = Self::edge(sig, self.log2_l, self.segment_size);
        let chunk = Self::chunk(sig, self.high_bits, self.chunk_mask);
        // chunk * self.segment_size * (2^log2_l + 2)
        let chunk_offset = chunk * ((self.segment_size << self.log2_l) + (self.segment_size << 1));
        self.values.get(edge[0] + chunk_offset)
            ^ self.values.get(edge[1] + chunk_offset)
            ^ self.values.get(edge[2] + chunk_offset)
    }

    #[inline(always)]
    pub fn get(&self, key: &T) -> usize {
        self.get_by_sig(&T::remap(key, self.seed))
    }

    pub fn len(&self) -> usize {
        self.num_keys
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn new<
        I: std::iter::IntoIterator<Item = T> + Clone,
        V: std::iter::IntoIterator<Item = usize> + Clone,
    >(
        keys: I,
        into_values: &mut V,
        bit_width: usize,
        pl: &mut Option<&mut ProgressLogger>,
    ) -> Function<T> {
        // Loop until success or duplicate detection
        let mut dup_count = 0;
        for seed in 0.. {
            if let Some(pl) = pl.as_mut() {
                pl.start("Reading input...")
            }

            let mut values = into_values.clone().into_iter();

            let mut sigs = keys
                .clone()
                .into_iter()
                .map(|x| {
                    (
                        T::remap(&x, seed),
                        values.next().expect("Not enough values"),
                    )
                })
                .collect::<Vec<_>>();

            if let Some(pl) = pl.as_mut() {
                pl.done_with_count(sigs.len());
            }

            if false && values.next().is_some() {
                // TODO
                panic!("Too many values");
            }

            let num_keys = sigs.len();

            let eps = 0.001;
            let mut high_bits = {
                let t = (sigs.len() as f64 * eps * eps / 2.0).ln();
                dbg!(t);
                if t > 0.0 {
                    ((t - t.ln()) / 2_f64.ln()).ceil()
                } else {
                    0.0
                }
            } as u32;

            dbg!(high_bits);

            let (l, c) = {
                loop {
                    let (_, l, c) = PARAMS
                        .iter()
                        .rev()
                        .filter(|(log_n, _l, _c)| (1 << log_n) <= (num_keys >> high_bits))
                        .copied()
                        .next()
                        .unwrap(); // first log_n is 0, so this is always valid
                    if high_bits == 0 || c <= 1.11 {
                        break (l, c);
                    }
                    high_bits -= 1;
                }
            };

            let log2_l = l.ilog2();
            dbg!(num_keys, high_bits, num_keys >> high_bits, log2_l, c);
            let num_chunks = 1 << high_bits;
            let chunk_mask = (1u32 << high_bits) - 1;

            if let Some(pl) = pl.as_mut() {
                pl.start("Sorting...")
            }
            sigs.par_sort_unstable();
            if let Some(pl) = pl.as_mut() {
                pl.done_with_count(num_keys);
            }

            if let Some(pl) = pl.as_mut() {
                pl.start("Checking for duplicates...")
            }

            let mut counts = vec![0; num_chunks];
            let mut dup = false;

            for w in sigs.windows(2) {
                counts[Self::chunk(&w[0].0, high_bits, chunk_mask)] += 1;
                if w[0].0 == w[1].0 {
                    dup = true;
                    break;
                }
            }

            counts[Self::chunk(&sigs[num_keys - 1].0, high_bits, chunk_mask)] += 1;

            if let Some(pl) = pl.as_mut() {
                pl.done_with_count(num_keys);
            }

            if dup {
                if dup_count >= 3 {
                    panic!(
                        "Duplicate keys (duplicate 128-bit signaturs with three different seeds)"
                    );
                }
                warn!("Duplicate 128-bit signature, trying again...");
                dup_count += 1;
                continue;
            }

            let mut cumul = vec![0; num_chunks + 1];
            for i in 0..num_chunks {
                cumul[i + 1] = cumul[i] + counts[i];
            }

            let segment_size =
                ((*counts.iter().max().unwrap() as f64 * c).ceil() as usize + l + 1) / (l + 2);
            let num_vertices = segment_size * (l + 2);
            eprintln!(
                "Size {:.2}%",
                (100.0 * (num_vertices * num_chunks) as f64) / (sigs.len() as f64 * c)
            );

            let data = CompactArray::new_atomic(bit_width, num_vertices * num_chunks);

            let chunk = AtomicUsize::new(0);
            let fail = AtomicBool::new(false);

            thread::scope(|s| {
                for _ in 0..num_chunks.min(num_cpus::get()) {
                    s.spawn(|| loop {
                        let chunk = chunk.fetch_add(1, Relaxed);
                        if chunk >= num_chunks {
                            break;
                        }
                        let sigs = &sigs[cumul[chunk]..cumul[chunk + 1]];

                        let mut pl = ProgressLogger::default();
                        pl.expected_updates = Some(sigs.len());

                        pl.start("Generating graph...");
                        let mut edge_lists = Vec::new();
                        edge_lists.resize_with(num_vertices, EdgeList::default);

                        sigs.iter().enumerate().for_each(|(edge_index, sig)| {
                            for &v in Self::edge(&sig.0, log2_l, segment_size).iter() {
                                edge_lists[v].add(edge_index);
                            }
                        });
                        pl.done_with_count(sigs.len());

                        let next = AtomicUsize::new(0);
                        let incr = 1024;

                        pl.start("Peeling...");
                        let stacks = Mutex::new(vec![]);

                        let mut stack = Vec::new();
                        loop {
                            if fail.load(Ordering::Relaxed) {
                                return;
                            }
                            let start = next.fetch_add(incr, Ordering::Relaxed);
                            if start >= num_vertices {
                                break;
                            }
                            for v in start..(start + incr).min(num_vertices) {
                                if edge_lists[v].degree() != 1 {
                                    continue;
                                }
                                let mut pos = stack.len();
                                let mut curr = stack.len();
                                stack.push(v);

                                while pos < stack.len() {
                                    let v = stack[pos];
                                    pos += 1;

                                    edge_lists[v].dec();
                                    if edge_lists[v].degree() != 0 {
                                        debug_assert_eq!(edge_lists[v].degree(), 0, "v = {}", v);
                                        continue; // Skip no longer useful entries
                                    }
                                    let edge_index = edge_lists[v].edge_index();

                                    stack[curr] = v;
                                    curr += 1;
                                    // Degree is necessarily 0
                                    for &x in
                                        Self::edge(&sigs[edge_index].0, log2_l, segment_size).iter()
                                    {
                                        if x != v {
                                            edge_lists[x].remove(edge_index);
                                            if edge_lists[x].degree() == 1 {
                                                stack.push(x);
                                            }
                                        }
                                    }
                                }
                                stack.truncate(curr);
                            }
                        }
                        if sigs.len() != stack.len() {
                            fail.store(true, Ordering::Relaxed);
                        }
                        stacks.lock().unwrap().push(stack);

                        pl.done_with_count(sigs.len());

                        pl.start("Assigning...");
                        let mut stacks = stacks.lock().unwrap();
                        while let Some(mut stack) = stacks.pop() {
                            while let Some(mut v) = stack.pop() {
                                let edge_index = edge_lists[v].edge_index();
                                let mut edge =
                                    Self::edge(&sigs[edge_index].0, log2_l, segment_size);
                                let chunk_offset = chunk * num_vertices;
                                v += chunk_offset;
                                edge.iter_mut().for_each(|v| {
                                    *v += chunk_offset;
                                });
                                let value = if v == edge[0] {
                                    data.get(edge[1], Relaxed) ^ data.get(edge[2], Relaxed)
                                } else if v == edge[1] {
                                    data.get(edge[0], Relaxed) ^ data.get(edge[2], Relaxed)
                                } else {
                                    data.get(edge[0], Relaxed) ^ data.get(edge[1], Relaxed)
                                };
                                // TODO
                                data.set(v, sigs[edge_index].1 ^ value, Relaxed);

                                assert_eq!(
                                    data.get(edge[0], Relaxed)
                                        ^ data.get(edge[1], Relaxed)
                                        ^ data.get(edge[2], Relaxed),
                                    sigs[edge_index].1
                                );
                            }
                        }
                        pl.done_with_count(sigs.len());
                    });
                }
            });

            if fail.load(Ordering::Relaxed) {
                warn!("Failed peeling, trying again...")
            } else {
                println!(
                    "bits/keys: {}",
                    data.len() as f64 * bit_width as f64 / sigs.len() as f64,
                );
                return Function {
                    seed,
                    log2_l,
                    high_bits,
                    chunk_mask,
                    num_keys: sigs.len(),
                    segment_size,
                    values: data.convert_to().unwrap(),
                    _phantom: std::marker::PhantomData,
                };
            }
        }
        unreachable!("There are infinite possible seeds.")
    }
}

fn _count_sort<T: Copy + Clone, F: Fn(&T) -> usize>(
    data: &mut [T],
    num_keys: usize,
    key: F,
) -> (Vec<usize>, Vec<usize>) {
    if num_keys == 1 {
        return (vec![data.len()], vec![0, data.len()]);
    }
    let mut counts = vec![0; num_keys];

    for sig in &*data {
        counts[key(sig)] += 1;
    }

    let mut cumul = Vec::new();
    cumul.resize(counts.len() + 1, 0);
    for i in 1..cumul.len() {
        cumul[i] += cumul[i - 1] + counts[i - 1];
    }
    let end = data.len() - counts.last().unwrap();

    let mut pos = cumul[1..].to_vec();
    let mut i = 0;
    while i < end {
        let mut sig = data[i];

        loop {
            let slot = key(&sig);
            pos[slot] -= 1;
            if pos[slot] <= i {
                break;
            }
            mem::swap(&mut data[pos[slot]], &mut sig);
        }
        data[i] = sig;
        i += counts[key(&sig)];
    }

    (counts, cumul)
}

#[inline(always)]
#[must_use]
pub const fn spooky_short_rehash(signature: &[u64; 2], seed: u64) -> [u64; 4] {
    spooky_short_mix([
        seed,
        SC_CONST.wrapping_add(signature[0]),
        SC_CONST.wrapping_add(signature[1]),
        SC_CONST,
    ])
}

#[derive(Parser, Debug)]
#[command(about = "Functions", long_about = None)]
#[clap(group(
            ArgGroup::new("input")
                .required(true)
                .args(&["filename", "n"]),
))]
struct Args {
    #[arg(short, long)]
    // A file containing UTF-8 keys, one per line.
    filename: Option<String>,
    // A name for the ε-serde serialized map.
    func: String,
    #[arg(short)]
    // Key bit width.
    w: usize,
    #[arg(short)]
    // Use the 64-bit keys [0..n).
    n: Option<usize>,
}

#[derive(Clone)]
struct FilenameIntoIterator<'a>(&'a String);

impl<'a> IntoIterator for FilenameIntoIterator<'a> {
    type Item = String;
    type IntoIter = std::iter::Map<
        std::io::Lines<BufReader<std::fs::File>>,
        fn(std::io::Result<String>) -> String,
    >;

    fn into_iter(self) -> Self::IntoIter {
        BufReader::new(std::fs::File::open(self.0).unwrap())
            .lines()
            .map(|line| line.unwrap())
    }
}

fn main() -> Result<()> {
    stderrlog::new()
        .verbosity(2)
        .timestamp(stderrlog::Timestamp::Second)
        .init()
        .unwrap();

    let args = Args::parse();

    let mut pl = ProgressLogger::default();

    if let Some(filename) = args.filename {
        let func = Function::<_>::new(
            FilenameIntoIterator(&filename),
            &mut (0..),
            args.w,
            &mut Some(&mut pl),
        );

        let file = std::fs::File::open(&filename)?;
        let keys = BufReader::new(file)
            .lines()
            .map(|line| line.unwrap())
            .take(10_000_000)
            .collect::<Vec<_>>();

        func.store(&args.func)?;
        let func = Function::<_>::load_mem(&args.func)?;
        pl.start("Querying...");
        for (index, key) in keys.iter().enumerate() {
            assert_eq!(index, func.get(key) as usize);
        }
        pl.done_with_count(keys.len());
    }

    if let Some(n) = args.n {
        let func = Function::<_, CompactArray<Vec<usize>>>::new(
            0..n as u64,
            &mut (0..),
            args.w,
            &mut Some(&mut pl),
        );
        func.store(&args.func)?;
        let func = Function::<u64, CompactArray<Vec<usize>>>::load_mmap(
            &args.func,
            epserde::deser::mem_case::Flags::TRANSPARENT_HUGE_PAGES,
        )?;
        pl.start("Querying...");
        for (index, key) in (0..n as u64).enumerate() {
            assert_eq!(index, func.get(&key) as usize);
        }
        pl.done_with_count(n);
    }
    Ok(())
}
