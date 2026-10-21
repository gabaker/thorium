//! Property tests over adversarial read schedules
//!
//! Every streaming defect this format has had reduces to one of two shapes:
//!
//! * a `poll_read` that returned `Ready` having filled nothing, which `AsyncRead` defines as
//!   end of file, so the caller saw a short file with no error; or
//! * a `poll_read` that threw away work it had already done when the reader underneath it
//!   stalled, which loses the tail of a file just as silently.
//!
//! Neither shape can occur when the reader is a `&[u8]`, because a slice never stalls and never
//! hands back less than it was asked for. That is why the unit tests in `src` never caught any
//! of them. This file drives both sides of a round trip through a reader that stalls and short
//! reads on a generated schedule, through a read buffer that can be a single byte wide.

use std::pin::Pin;
use std::task::{Context, Poll};

use proptest::prelude::*;
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

use cart_rs::{CartKey, CartStream, CartVersion, UncartStream, V2Options};
// only the bit flipping property needs to know where the plaintext footer starts
#[cfg(feature = "v2")]
use cart_rs::FOOTER_LEN;

/// Every `CaRT` version this build can actually write
///
/// The feature matrix is part of what is under test, so a build with one codec compiled out
/// runs the same properties over the codec it kept rather than failing to compile.
const VERSIONS: &[CartVersion] = &[
    #[cfg(feature = "v1")]
    CartVersion::V1,
    #[cfg(feature = "v2")]
    CartVersion::V2,
];

/// Build the key every test carts with
fn key() -> CartKey {
    CartKey::from_password("SecretCornIsBest").expect("the test password is a valid key")
}

/// How brutally to drive one side of a round trip
#[derive(Debug, Clone, Copy)]
struct Schedule {
    /// The most bytes the reader underneath hands over in a single poll
    chunk: usize,
    /// Stall on every `stall_every`th poll, or never if this is zero
    ///
    /// This is never 1. A reader that stalls on every single poll never makes progress, which
    /// hangs the test rather than testing anything.
    stall_every: u32,
    /// The size of the buffer we read into
    buf: usize,
}

impl Schedule {
    /// Build a schedule that does nothing awkward
    ///
    /// This is for the properties that are about the format rather than about the read
    /// schedule, so that a failure points at the thing being tested.
    fn simple() -> Self {
        Schedule {
            chunk: 64 * 1024,
            stall_every: 0,
            buf: 64 * 1024,
        }
    }
}

/// A reader that hands out its data in small pieces and stalls on a schedule
struct StallingReader<'a> {
    /// The bytes left to hand out
    data: &'a [u8],
    /// The schedule to hand them out on
    schedule: Schedule,
    /// How many times this reader has been polled
    polls: u32,
}

impl<'a> StallingReader<'a> {
    /// Wrap some bytes in a reader that stalls and short reads
    ///
    /// # Arguments
    ///
    /// * `data` - The bytes to hand out
    /// * `schedule` - How awkwardly to hand them out
    fn new(data: &'a [u8], schedule: Schedule) -> Self {
        StallingReader {
            data,
            schedule,
            polls: 0,
        }
    }
}

impl AsyncRead for StallingReader<'_> {
    /// Hand over at most one chunk, or stall if this poll is a scheduled stall
    ///
    /// # Arguments
    ///
    /// * `cx` - The context to register a waker against when stalling
    /// * `buf` - The buffer to read into
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // this reader holds nothing pinned, so it can be unpinned freely
        let this = self.get_mut();
        // count this poll
        this.polls = this.polls.wrapping_add(1);
        // stall if this poll is a scheduled stall, waking ourselves so the runtime comes
        // straight back rather than parking forever. A period of 1 is treated as never rather
        // than always, because a reader that stalls on every poll makes no progress and would
        // spin the test instead of testing anything.
        if this.schedule.stall_every > 1 && this.polls.is_multiple_of(this.schedule.stall_every) {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        // hand over at most a chunk, and never more than the caller asked for
        let take = this
            .schedule
            .chunk
            .min(buf.remaining())
            .min(this.data.len());
        buf.put_slice(&this.data[..take]);
        this.data = &this.data[take..];
        Poll::Ready(Ok(()))
    }
}

/// Read a stream to the end through a fixed size buffer
///
/// The buffer size is the point: a one byte buffer means the stream can never hand back more
/// than one byte per poll, which is the shape that breaks any code assuming a header, a length
/// prefix, or a footer arrives contiguously.
///
/// # Arguments
///
/// * `reader` - The stream to read to the end
/// * `buf_size` - The size of the buffer to read through
async fn drain<R: AsyncRead + Unpin>(mut reader: R, buf_size: usize) -> std::io::Result<Vec<u8>> {
    // read through a buffer of exactly the size we were asked for
    let mut scratch = vec![0u8; buf_size];
    let mut out = Vec::new();
    loop {
        let read = reader.read(&mut scratch).await?;
        // a zero length read is the only end of file signal AsyncRead has
        if read == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&scratch[..read]);
    }
}

/// Cart some plaintext through [`CartStream`] under an adversarial read schedule
///
/// # Arguments
///
/// * `plain` - The plaintext to cart
/// * `version` - The `CaRT` version to write
/// * `log2` - The base 2 logarithm of the V2 block size, ignored when writing V1
/// * `schedule` - How awkwardly to feed the carter its plaintext
async fn cart(
    plain: &[u8],
    version: CartVersion,
    log2: u8,
    schedule: Schedule,
) -> std::io::Result<Vec<u8>> {
    // wrap the plaintext in a reader that stalls and short reads
    let reader = StallingReader::new(plain, schedule);
    // build a carter with a small block size so block boundaries are crossed without needing
    // megabytes of input in every generated case
    let stream = CartStream::builder(key(), reader)
        .version(version)
        .v2_options(V2Options {
            zstd_level: 3,
            block_size_log2: log2,
        })
        .build()?;
    drain(stream, schedule.buf).await
}

/// Uncart some carted bytes through [`UncartStream`] under an adversarial read schedule
///
/// # Arguments
///
/// * `carted` - The carted bytes to uncart
/// * `schedule` - How awkwardly to feed the uncarter its cart
async fn uncart(carted: &[u8], schedule: Schedule) -> std::io::Result<Vec<u8>> {
    // wrap the cart in a reader that stalls and short reads
    let reader = StallingReader::new(carted, schedule);
    drain(UncartStream::new(reader), schedule.buf).await
}

/// Run a future to completion on a single threaded runtime
///
/// Single threaded on purpose. The stall schedule is then deterministic, so a failing case
/// shrinks to something that reproduces instead of to something that raced once.
///
/// # Arguments
///
/// * `future` - The future to run
fn run<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("failed to build the test runtime")
        .block_on(future)
}

/// Build a strategy over the `CaRT` versions this build can write
fn version() -> impl Strategy<Value = CartVersion> {
    prop::sample::select(VERSIONS.to_vec())
}

/// Build a strategy over read schedules
///
/// The sizes are drawn from three overlapping ranges rather than one wide one so that the
/// pathological end gets visited often. Drawing uniformly from `1..=65536` would put a one byte
/// buffer in roughly one case in sixty five thousand, which is never.
fn schedule() -> impl Strategy<Value = Schedule> {
    let size = prop_oneof![
        Just(1_usize),
        1_usize..=64,
        1_usize..=4096,
        1_usize..=65_536,
    ];
    // 0 means never stall; 1 is excluded because a reader that stalls on every poll makes no
    // progress at all and would hang rather than test anything
    let stall_every = prop_oneof![Just(0_u32), 2_u32..=4];
    (size.clone(), stall_every, size).prop_map(|(chunk, stall_every, buf)| Schedule {
        chunk,
        stall_every,
        buf,
    })
}

/// How far past the decompressor's output room a compressible payload is drawn
///
/// The V1 decoder always offers `miniz` at least 64 KiB of room, so a file that decompresses to
/// less than that can never fill the slice it was handed and can never leave anything parked in
/// `miniz`'s internal dictionary. A payload has to clear that floor by a wide margin before the
/// interesting path is reachable at all. It costs almost nothing to go there: 256 KiB of one
/// repeated byte carts down to a few hundred bytes.
const EXPANSION_FLOOR: usize = 256 * 1024;

/// Build a strategy over payloads, compressible and not
///
/// Drawing every byte from `any::<u8>()` gives data that `DEFLATE` cannot shrink, so the
/// decompressor can never produce meaningfully more output than the room it was handed. Half of
/// the streaming surface only exists on the other side of that: the decompressor parks finished
/// output in its own dictionary when the slice it was given fills up, and a decoder that does not
/// go back for it loses the tail. The three compressible shapes here reach it; `max` only bounds
/// the incompressible one, which is the only shape whose cost scales with its length.
fn payload(min: usize, max: usize) -> impl Strategy<Value = Vec<u8>> {
    // compressible payloads are cheap to carry, so they ignore `max` and go past the room instead
    let big = min..max.max(EXPANSION_FLOOR);
    prop_oneof![
        // incompressible, which is what this file used to test exclusively
        prop::collection::vec(any::<u8>(), min..max),
        // one enormous run, the most compressible thing there is
        (any::<u8>(), big.clone()).prop_map(|(byte, len)| vec![byte; len]),
        // a short pattern repeated, which compresses very well but not perfectly
        (prop::collection::vec(any::<u8>(), 1..64), big.clone()).prop_map(|(seed, len)| seed
            .into_iter()
            .cycle()
            .take(len)
            .collect()),
        // runs broken up by noise, so the expansion ratio swings around mid stream
        (prop::collection::vec(any::<u8>(), 1..64), big).prop_map(|(seed, len)| (0..len)
            .map(|i| if i % 64 < 56 { 0 } else { seed[i % seed.len()] })
            .collect()),
    ]
}

proptest! {
    // these cases carry up to 256 KiB through a possibly one byte wide buffer, so they are far
    // more expensive than a default proptest case; the count is tuned to keep the suite quick
    // enough to gate CI on. Raise it with PROPTEST_CASES when hunting rather than here; the
    // seeds in `streaming_properties.proptest-regressions` are what make past failures
    // deterministic at this count
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// A file survives a round trip no matter how the bytes arrive on either side
    ///
    /// This single property covers every historical streaming defect at once: the lost tail on
    /// the uncommitted return path, the phantom end of file when the decompressor wanted more
    /// input, the missing waker on the stalled path, the hard failure when a header arrived in
    /// pieces, and the output stranded inside the decompressor's own dictionary.
    #[test]
    fn a_carted_file_always_uncarts_to_the_original(
        plain in payload(0, 65_536),
        version in version(),
        log2 in 10_u8..=13,
        write in schedule(),
        read in schedule(),
    ) {
        // cart it, feeding the carter its plaintext on one awkward schedule
        let carted = run(cart(&plain, version, log2, write))?;
        // and uncart it, feeding the uncarter its cart on a different awkward schedule
        let recovered = run(uncart(&carted, read))?;
        prop_assert_eq!(recovered.len(), plain.len(), "recovered the wrong number of bytes");
        prop_assert!(recovered == plain, "the round trip changed the file's contents");
    }

    /// The read schedule never changes the bytes that come out of the carter
    ///
    /// A carter that emitted different output depending on how its input was delivered would
    /// make the golden vectors meaningless, and would mean two uploads of the same file could
    /// disagree. V2 salts every file with fresh randomness, so only V1 can be compared byte for
    /// byte; for V2 the length is the strongest claim available.
    #[test]
    fn the_read_schedule_never_changes_the_output(
        plain in payload(0, 32_768),
        version in version(),
        log2 in 10_u8..=13,
        awkward in schedule(),
    ) {
        // cart it once with nothing awkward happening
        let baseline = run(cart(&plain, version, log2, Schedule::simple()))?;
        // and once through a reader that stalls and dribbles
        let dribbled = run(cart(&plain, version, log2, awkward))?;
        if version == CartVersion::V1 {
            prop_assert!(baseline == dribbled, "V1 output depended on the read schedule");
        } else {
            prop_assert_eq!(
                baseline.len(),
                dribbled.len(),
                "V2 output length depended on the read schedule",
            );
        }
    }

    /// A truncated cart is an error rather than a short read
    ///
    /// This is the property that a failed upload depends on. An interrupted multipart upload
    /// leaves a truncated object in s3, and the dangerous outcome is not an error, it is a
    /// clean end of file after however many bytes happened to survive. That looks exactly like
    /// a successful download of a corrupted file.
    #[test]
    fn a_truncated_cart_is_an_error_rather_than_a_short_read(
        plain in payload(1, 16_384),
        version in version(),
        cut_at in any::<prop::sample::Index>(),
        read in schedule(),
    ) {
        let carted = run(cart(&plain, version, 12, Schedule::simple()))?;
        // cut the file off somewhere strictly before its end
        let cut = cut_at.index(carted.len());
        let result = run(uncart(&carted[..cut], read));
        prop_assert!(
            result.is_err(),
            "a cart truncated from {} to {cut} bytes uncarted cleanly to {:?} bytes",
            carted.len(),
            result.map(|out| out.len()),
        );
    }
}

proptest! {
    /// Arbitrary bytes are rejected rather than crashing or hanging
    ///
    /// Uncarting is reachable from an unauthenticated download path, so the only acceptable
    /// outcomes for a hostile file are a clean error or a clean read. A panic, an unbounded
    /// allocation driven by a length field, or a hang are all denial of service.
    #[test]
    fn uncarting_arbitrary_bytes_never_panics(
        raw in prop::collection::vec(any::<u8>(), 0..4096),
        prefixed in any::<bool>(),
        read in schedule(),
    ) {
        // pure noise almost always dies on the magic number, which tests one branch very well
        // and nothing else, so half the cases get a valid magic number glued on the front to
        // push the parser into the version, key and length fields behind it
        let input = if prefixed {
            let mut input = b"CART".to_vec();
            input.extend_from_slice(&raw);
            input
        } else {
            raw
        };
        // the assertion is that this returns at all
        let _ = run(uncart(&input, read));
    }

    /// A cart with bytes overwritten in it is rejected rather than crashing
    ///
    /// Unlike the noise above, this reaches the codecs: the header parses, so the reader
    /// commits to a version and starts decrypting and decompressing attacker controlled bytes.
    #[test]
    fn uncarting_a_corrupted_cart_never_panics(
        plain in prop::collection::vec(any::<u8>(), 0..8192),
        version in version(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8),
        read in schedule(),
    ) {
        let mut carted = run(cart(&plain, version, 12, Schedule::simple()))?;
        // scribble over a handful of bytes anywhere in the file
        for (at, byte) in edits {
            let index = at.index(carted.len());
            carted[index] = byte;
        }
        // the assertion is that this returns at all
        let _ = run(uncart(&carted, read));
    }
}

#[cfg(feature = "v2")]
proptest! {
    /// Flipping any single bit of a V2 cart is always detected
    ///
    /// This is the test that proves the AEAD does something. Every byte between the magic
    /// number and the plaintext footer is either encrypted under GCM or fed to it as additional
    /// authenticated data, so a single bit flip anywhere in that span has to fail the tag check
    /// or fail a structural check. Silently returning different bytes is the failure this rules
    /// out.
    ///
    /// There is deliberately no V1 equivalent. V1 is `RC4` over DEFLATE with no authentication
    /// at all, so a bit flip there can and does produce different plaintext without any error.
    /// That is a property of the official format, not something this crate can fix, and it is
    /// the reason V2 exists.
    #[test]
    fn flipping_any_bit_of_a_v2_cart_is_detected(
        plain in prop::collection::vec(any::<u8>(), 1..8192),
        at in any::<prop::sample::Index>(),
        bit in 0_u8..8,
    ) {
        let mut carted = run(cart(&plain, CartVersion::V2, 12, Schedule::simple()))?;
        // pick a byte in the authenticated span, which is everything after the magic number and
        // before the plaintext footer
        let span = 4..carted.len() - FOOTER_LEN;
        let index = at.index(span.len()) + span.start;
        carted[index] ^= 1 << bit;
        // that must be an error, not merely different bytes
        let result = run(uncart(&carted, Schedule::simple()));
        prop_assert!(
            result.is_err(),
            "flipping bit {bit} of byte {index} of {} was not detected",
            carted.len(),
        );
    }
}
