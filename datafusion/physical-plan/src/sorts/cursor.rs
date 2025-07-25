// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::cmp::Ordering;
use std::sync::Arc;

use arrow::array::{
    types::ByteArrayType, Array, ArrowPrimitiveType, GenericByteArray,
    GenericByteViewArray, OffsetSizeTrait, PrimitiveArray, StringViewArray,
};
use arrow::buffer::{Buffer, OffsetBuffer, ScalarBuffer};
use arrow::compute::SortOptions;
use arrow::datatypes::ArrowNativeTypeOp;
use arrow::row::Rows;
use datafusion_execution::memory_pool::MemoryReservation;

/// A comparable collection of values for use with [`Cursor`]
///
/// This is a trait as there are several specialized implementations, such as for
/// single columns or for normalized multi column keys ([`Rows`])
pub trait CursorValues {
    fn len(&self) -> usize;

    /// Returns true if `l[l_idx] == r[r_idx]`
    fn eq(l: &Self, l_idx: usize, r: &Self, r_idx: usize) -> bool;

    /// Returns true if `row[idx] == row[idx - 1]`
    /// Given `idx` should be greater than 0
    fn eq_to_previous(cursor: &Self, idx: usize) -> bool;

    /// Returns comparison of `l[l_idx]` and `r[r_idx]`
    fn compare(l: &Self, l_idx: usize, r: &Self, r_idx: usize) -> Ordering;
}

/// A comparable cursor, used by sort operations
///
/// A `Cursor` is a pointer into a collection of rows, stored in
/// [`CursorValues`]
///
/// ```text
///
/// ┌───────────────────────┐
/// │                       │           ┌──────────────────────┐
/// │ ┌─────────┐ ┌─────┐   │    ─ ─ ─ ─│      Cursor<T>       │
/// │ │    1    │ │  A  │   │   │       └──────────────────────┘
/// │ ├─────────┤ ├─────┤   │
/// │ │    2    │ │  A  │◀─ ┼ ─ ┘          Cursor<T> tracks an
/// │ └─────────┘ └─────┘   │                offset within a
/// │     ...       ...     │                  CursorValues
/// │                       │
/// │ ┌─────────┐ ┌─────┐   │
/// │ │    3    │ │  E  │   │
/// │ └─────────┘ └─────┘   │
/// │                       │
/// │     CursorValues      │
/// └───────────────────────┘
///
///
/// Store logical rows using
/// one of several  formats,
/// with specialized
/// implementations
/// depending on the column
/// types
#[derive(Debug)]
pub struct Cursor<T: CursorValues> {
    offset: usize,
    values: T,
}

impl<T: CursorValues> Cursor<T> {
    /// Create a [`Cursor`] from the given [`CursorValues`]
    pub fn new(values: T) -> Self {
        Self { offset: 0, values }
    }

    /// Returns true if there are no more rows in this cursor
    pub fn is_finished(&self) -> bool {
        self.offset == self.values.len()
    }

    /// Advance the cursor, returning the previous row index
    pub fn advance(&mut self) -> usize {
        let t = self.offset;
        self.offset += 1;
        t
    }

    pub fn is_eq_to_prev_one(&self, prev_cursor: Option<&Cursor<T>>) -> bool {
        if self.offset > 0 {
            self.is_eq_to_prev_row()
        } else if let Some(prev_cursor) = prev_cursor {
            self.is_eq_to_prev_row_in_prev_batch(prev_cursor)
        } else {
            false
        }
    }
}

impl<T: CursorValues> PartialEq for Cursor<T> {
    fn eq(&self, other: &Self) -> bool {
        T::eq(&self.values, self.offset, &other.values, other.offset)
    }
}

impl<T: CursorValues> Cursor<T> {
    fn is_eq_to_prev_row(&self) -> bool {
        T::eq_to_previous(&self.values, self.offset)
    }

    fn is_eq_to_prev_row_in_prev_batch(&self, other: &Self) -> bool {
        assert_eq!(self.offset, 0);
        T::eq(
            &self.values,
            self.offset,
            &other.values,
            other.values.len() - 1,
        )
    }
}

impl<T: CursorValues> Eq for Cursor<T> {}

impl<T: CursorValues> PartialOrd for Cursor<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: CursorValues> Ord for Cursor<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        T::compare(&self.values, self.offset, &other.values, other.offset)
    }
}

/// Implements [`CursorValues`] for [`Rows`]
///
/// Used for sorting when there are multiple columns in the sort key
#[derive(Debug)]
pub struct RowValues {
    rows: Arc<Rows>,

    /// Tracks for the memory used by in the `Rows` of this
    /// cursor. Freed on drop
    _reservation: MemoryReservation,
}

impl RowValues {
    /// Create a new [`RowValues`] from `rows` and a `reservation`
    /// that tracks its memory. There must be at least one row
    ///
    /// Panics if the reservation is not for exactly `rows.size()`
    /// bytes or if `rows` is empty.
    pub fn new(rows: Arc<Rows>, reservation: MemoryReservation) -> Self {
        assert_eq!(
            rows.size(),
            reservation.size(),
            "memory reservation mismatch"
        );
        assert!(rows.num_rows() > 0);
        Self {
            rows,
            _reservation: reservation,
        }
    }
}

impl CursorValues for RowValues {
    fn len(&self) -> usize {
        self.rows.num_rows()
    }

    fn eq(l: &Self, l_idx: usize, r: &Self, r_idx: usize) -> bool {
        l.rows.row(l_idx) == r.rows.row(r_idx)
    }

    fn eq_to_previous(cursor: &Self, idx: usize) -> bool {
        assert!(idx > 0);
        cursor.rows.row(idx) == cursor.rows.row(idx - 1)
    }

    fn compare(l: &Self, l_idx: usize, r: &Self, r_idx: usize) -> Ordering {
        l.rows.row(l_idx).cmp(&r.rows.row(r_idx))
    }
}

/// An [`Array`] that can be converted into [`CursorValues`]
pub trait CursorArray: Array + 'static {
    type Values: CursorValues;

    fn values(&self) -> Self::Values;
}

impl<T: ArrowPrimitiveType> CursorArray for PrimitiveArray<T> {
    type Values = PrimitiveValues<T::Native>;

    fn values(&self) -> Self::Values {
        PrimitiveValues(self.values().clone())
    }
}

#[derive(Debug)]
pub struct PrimitiveValues<T: ArrowNativeTypeOp>(ScalarBuffer<T>);

impl<T: ArrowNativeTypeOp> CursorValues for PrimitiveValues<T> {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn eq(l: &Self, l_idx: usize, r: &Self, r_idx: usize) -> bool {
        l.0[l_idx].is_eq(r.0[r_idx])
    }

    fn eq_to_previous(cursor: &Self, idx: usize) -> bool {
        assert!(idx > 0);
        cursor.0[idx].is_eq(cursor.0[idx - 1])
    }

    fn compare(l: &Self, l_idx: usize, r: &Self, r_idx: usize) -> Ordering {
        l.0[l_idx].compare(r.0[r_idx])
    }
}

pub struct ByteArrayValues<T: OffsetSizeTrait> {
    offsets: OffsetBuffer<T>,
    values: Buffer,
}

impl<T: OffsetSizeTrait> ByteArrayValues<T> {
    fn value(&self, idx: usize) -> &[u8] {
        assert!(idx < self.len());
        // Safety: offsets are valid and checked bounds above
        unsafe {
            let start = self.offsets.get_unchecked(idx).as_usize();
            let end = self.offsets.get_unchecked(idx + 1).as_usize();
            self.values.get_unchecked(start..end)
        }
    }
}

impl<T: OffsetSizeTrait> CursorValues for ByteArrayValues<T> {
    fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    fn eq(l: &Self, l_idx: usize, r: &Self, r_idx: usize) -> bool {
        l.value(l_idx) == r.value(r_idx)
    }

    fn eq_to_previous(cursor: &Self, idx: usize) -> bool {
        assert!(idx > 0);
        cursor.value(idx) == cursor.value(idx - 1)
    }

    fn compare(l: &Self, l_idx: usize, r: &Self, r_idx: usize) -> Ordering {
        l.value(l_idx).cmp(r.value(r_idx))
    }
}

impl<T: ByteArrayType> CursorArray for GenericByteArray<T> {
    type Values = ByteArrayValues<T::Offset>;

    fn values(&self) -> Self::Values {
        ByteArrayValues {
            offsets: self.offsets().clone(),
            values: self.values().clone(),
        }
    }
}

impl CursorArray for StringViewArray {
    type Values = StringViewArray;
    fn values(&self) -> Self {
        self.gc()
    }
}

/// Todo use arrow-rs side api after: <https://github.com/apache/arrow-rs/pull/7748> released
/// Builds a 128-bit composite key for an inline value:
///
/// - High 96 bits: the inline data in big-endian byte order (for correct lexicographical sorting).
/// - Low  32 bits: the length in big-endian byte order, acting as a tiebreaker so shorter strings
///   (or those with fewer meaningful bytes) always numerically sort before longer ones.
///
/// This function extracts the length and the 12-byte inline string data from the raw
/// little-endian `u128` representation, converts them to big-endian ordering, and packs them
/// into a single `u128` value suitable for fast, branchless comparisons.
///
/// ### Why include length?
///
/// A pure 96-bit content comparison can’t distinguish between two values whose inline bytes
/// compare equal—either because one is a true prefix of the other or because zero-padding
/// hides extra bytes. By tucking the 32-bit length into the lower bits, a single `u128` compare
/// handles both content and length in one go.
///
/// Example: comparing "bar" (3 bytes) vs "bar\0" (4 bytes)
///
/// | String     | Bytes 0–4 (length LE) | Bytes 4–16 (data + padding)    |
/// |------------|-----------------------|---------------------------------|
/// | `"bar"`   | `03 00 00 00`         | `62 61 72` + 9 × `00`           |
/// | `"bar\0"`| `04 00 00 00`         | `62 61 72 00` + 8 × `00`        |
///
/// Both inline parts become `62 61 72 00…00`, so they tie on content. The length field
/// then differentiates:
///
/// ```text
/// key("bar")   = 0x0000000000000000000062617200000003
/// key("bar\0") = 0x0000000000000000000062617200000004
/// ⇒ key("bar") < key("bar\0")
/// ```
#[inline(always)]
pub fn inline_key_fast(raw: u128) -> u128 {
    // Convert the raw u128 (little-endian) into bytes for manipulation
    let raw_bytes = raw.to_le_bytes();

    // Extract the length (first 4 bytes), convert to big-endian u32, and promote to u128
    let len_le = &raw_bytes[0..4];
    let len_be = u32::from_le_bytes(len_le.try_into().unwrap()).to_be() as u128;

    // Extract the inline string bytes (next 12 bytes), place them into the lower 12 bytes of a 16-byte array,
    // padding the upper 4 bytes with zero to form a little-endian u128 value
    let mut inline_bytes = [0u8; 16];
    inline_bytes[4..16].copy_from_slice(&raw_bytes[4..16]);

    // Convert to big-endian to ensure correct lexical ordering
    let inline_u128 = u128::from_le_bytes(inline_bytes).to_be();

    // Shift right by 32 bits to discard the zero padding (upper 4 bytes),
    // so that the inline string occupies the high 96 bits
    let inline_part = inline_u128 >> 32;

    // Combine the inline string part (high 96 bits) and length (low 32 bits) into the final key
    (inline_part << 32) | len_be
}

impl CursorValues for StringViewArray {
    fn len(&self) -> usize {
        self.views().len()
    }

    #[inline(always)]
    fn eq(l: &Self, l_idx: usize, r: &Self, r_idx: usize) -> bool {
        // SAFETY: Both l_idx and r_idx are guaranteed to be within bounds,
        // and any null-checks are handled in the outer layers.
        // Fast path: Compare the lengths before full byte comparison.
        let l_view = unsafe { l.views().get_unchecked(l_idx) };
        let r_view = unsafe { r.views().get_unchecked(r_idx) };

        if l.data_buffers().is_empty() && r.data_buffers().is_empty() {
            return l_view == r_view;
        }

        let l_len = *l_view as u32;
        let r_len = *r_view as u32;
        if l_len != r_len {
            return false;
        }

        unsafe { GenericByteViewArray::compare_unchecked(l, l_idx, r, r_idx).is_eq() }
    }

    #[inline(always)]
    fn eq_to_previous(cursor: &Self, idx: usize) -> bool {
        // SAFETY: The caller guarantees that idx > 0 and the indices are valid.
        // Already checked it in is_eq_to_prev_one function
        // Fast path: Compare the lengths of the current and previous views.
        let l_view = unsafe { cursor.views().get_unchecked(idx) };
        let r_view = unsafe { cursor.views().get_unchecked(idx - 1) };
        if cursor.data_buffers().is_empty() {
            return l_view == r_view;
        }

        let l_len = *l_view as u32;
        let r_len = *r_view as u32;

        if l_len != r_len {
            return false;
        }

        unsafe {
            GenericByteViewArray::compare_unchecked(cursor, idx, cursor, idx - 1).is_eq()
        }
    }

    #[inline(always)]
    fn compare(l: &Self, l_idx: usize, r: &Self, r_idx: usize) -> Ordering {
        // SAFETY: Prior assertions guarantee that l_idx and r_idx are valid indices.
        // Null-checks are assumed to have been handled in the wrapper (e.g., ArrayValues).
        // And the bound is checked in is_finished, it is safe to call get_unchecked
        if l.data_buffers().is_empty() && r.data_buffers().is_empty() {
            let l_view = unsafe { l.views().get_unchecked(l_idx) };
            let r_view = unsafe { r.views().get_unchecked(r_idx) };
            return inline_key_fast(*l_view).cmp(&inline_key_fast(*r_view));
        }

        unsafe { GenericByteViewArray::compare_unchecked(l, l_idx, r, r_idx) }
    }
}

/// A collection of sorted, nullable [`CursorValues`]
///
/// Note: comparing cursors with different `SortOptions` will yield an arbitrary ordering
#[derive(Debug)]
pub struct ArrayValues<T: CursorValues> {
    values: T,
    // If nulls first, the first non-null index
    // Otherwise, the first null index
    null_threshold: usize,
    options: SortOptions,

    /// Tracks the memory used by the values array,
    /// freed on drop.
    _reservation: MemoryReservation,
}

impl<T: CursorValues> ArrayValues<T> {
    /// Create a new [`ArrayValues`] from the provided `values` sorted according
    /// to `options`.
    ///
    /// Panics if the array is empty
    pub fn new<A: CursorArray<Values = T>>(
        options: SortOptions,
        array: &A,
        reservation: MemoryReservation,
    ) -> Self {
        assert!(array.len() > 0, "Empty array passed to FieldCursor");
        let null_threshold = match options.nulls_first {
            true => array.null_count(),
            false => array.len() - array.null_count(),
        };

        Self {
            values: array.values(),
            null_threshold,
            options,
            _reservation: reservation,
        }
    }

    fn is_null(&self, idx: usize) -> bool {
        (idx < self.null_threshold) == self.options.nulls_first
    }
}

impl<T: CursorValues> CursorValues for ArrayValues<T> {
    fn len(&self) -> usize {
        self.values.len()
    }

    fn eq(l: &Self, l_idx: usize, r: &Self, r_idx: usize) -> bool {
        match (l.is_null(l_idx), r.is_null(r_idx)) {
            (true, true) => true,
            (false, false) => T::eq(&l.values, l_idx, &r.values, r_idx),
            _ => false,
        }
    }

    fn eq_to_previous(cursor: &Self, idx: usize) -> bool {
        assert!(idx > 0);
        match (cursor.is_null(idx), cursor.is_null(idx - 1)) {
            (true, true) => true,
            (false, false) => T::eq(&cursor.values, idx, &cursor.values, idx - 1),
            _ => false,
        }
    }

    fn compare(l: &Self, l_idx: usize, r: &Self, r_idx: usize) -> Ordering {
        match (l.is_null(l_idx), r.is_null(r_idx)) {
            (true, true) => Ordering::Equal,
            (true, false) => match l.options.nulls_first {
                true => Ordering::Less,
                false => Ordering::Greater,
            },
            (false, true) => match l.options.nulls_first {
                true => Ordering::Greater,
                false => Ordering::Less,
            },
            (false, false) => match l.options.descending {
                true => T::compare(&r.values, r_idx, &l.values, l_idx),
                false => T::compare(&l.values, l_idx, &r.values, r_idx),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use arrow::array::GenericBinaryArray;
    use datafusion_execution::memory_pool::{
        GreedyMemoryPool, MemoryConsumer, MemoryPool,
    };
    use std::sync::Arc;

    use super::*;

    fn new_primitive(
        options: SortOptions,
        values: ScalarBuffer<i32>,
        null_count: usize,
    ) -> Cursor<ArrayValues<PrimitiveValues<i32>>> {
        let null_threshold = match options.nulls_first {
            true => null_count,
            false => values.len() - null_count,
        };

        let memory_pool: Arc<dyn MemoryPool> = Arc::new(GreedyMemoryPool::new(10000));
        let consumer = MemoryConsumer::new("test");
        let reservation = consumer.register(&memory_pool);

        let values = ArrayValues {
            values: PrimitiveValues(values),
            null_threshold,
            options,
            _reservation: reservation,
        };

        Cursor::new(values)
    }

    #[test]
    fn test_primitive_nulls_first() {
        let options = SortOptions {
            descending: false,
            nulls_first: true,
        };

        let buffer = ScalarBuffer::from(vec![i32::MAX, 1, 2, 3]);
        let mut a = new_primitive(options, buffer, 1);
        let buffer = ScalarBuffer::from(vec![1, 2, -2, -1, 1, 9]);
        let mut b = new_primitive(options, buffer, 2);

        // NULL == NULL
        assert_eq!(a.cmp(&b), Ordering::Equal);
        assert_eq!(a, b);

        // NULL == NULL
        b.advance();
        assert_eq!(a.cmp(&b), Ordering::Equal);
        assert_eq!(a, b);

        // NULL < -2
        b.advance();
        assert_eq!(a.cmp(&b), Ordering::Less);

        // 1 > -2
        a.advance();
        assert_eq!(a.cmp(&b), Ordering::Greater);

        // 1 > -1
        b.advance();
        assert_eq!(a.cmp(&b), Ordering::Greater);

        // 1 == 1
        b.advance();
        assert_eq!(a.cmp(&b), Ordering::Equal);
        assert_eq!(a, b);

        // 9 > 1
        b.advance();
        assert_eq!(a.cmp(&b), Ordering::Less);

        // 9 > 2
        a.advance();
        assert_eq!(a.cmp(&b), Ordering::Less);

        let options = SortOptions {
            descending: false,
            nulls_first: false,
        };

        let buffer = ScalarBuffer::from(vec![0, 1, i32::MIN, i32::MAX]);
        let mut a = new_primitive(options, buffer, 2);
        let buffer = ScalarBuffer::from(vec![-1, i32::MAX, i32::MIN]);
        let mut b = new_primitive(options, buffer, 2);

        // 0 > -1
        assert_eq!(a.cmp(&b), Ordering::Greater);

        // 0 < NULL
        b.advance();
        assert_eq!(a.cmp(&b), Ordering::Less);

        // 1 < NULL
        a.advance();
        assert_eq!(a.cmp(&b), Ordering::Less);

        // NULL = NULL
        a.advance();
        assert_eq!(a.cmp(&b), Ordering::Equal);
        assert_eq!(a, b);

        let options = SortOptions {
            descending: true,
            nulls_first: false,
        };

        let buffer = ScalarBuffer::from(vec![6, 1, i32::MIN, i32::MAX]);
        let mut a = new_primitive(options, buffer, 3);
        let buffer = ScalarBuffer::from(vec![67, -3, i32::MAX, i32::MIN]);
        let mut b = new_primitive(options, buffer, 2);

        // 6 > 67
        assert_eq!(a.cmp(&b), Ordering::Greater);

        // 6 < -3
        b.advance();
        assert_eq!(a.cmp(&b), Ordering::Less);

        // 6 < NULL
        b.advance();
        assert_eq!(a.cmp(&b), Ordering::Less);

        // 6 < NULL
        b.advance();
        assert_eq!(a.cmp(&b), Ordering::Less);

        // NULL == NULL
        a.advance();
        assert_eq!(a.cmp(&b), Ordering::Equal);
        assert_eq!(a, b);

        let options = SortOptions {
            descending: true,
            nulls_first: true,
        };

        let buffer = ScalarBuffer::from(vec![i32::MIN, i32::MAX, 6, 3]);
        let mut a = new_primitive(options, buffer, 2);
        let buffer = ScalarBuffer::from(vec![i32::MAX, 4546, -3]);
        let mut b = new_primitive(options, buffer, 1);

        // NULL == NULL
        assert_eq!(a.cmp(&b), Ordering::Equal);
        assert_eq!(a, b);

        // NULL == NULL
        a.advance();
        assert_eq!(a.cmp(&b), Ordering::Equal);
        assert_eq!(a, b);

        // NULL < 4546
        b.advance();
        assert_eq!(a.cmp(&b), Ordering::Less);

        // 6 > 4546
        a.advance();
        assert_eq!(a.cmp(&b), Ordering::Greater);

        // 6 < -3
        b.advance();
        assert_eq!(a.cmp(&b), Ordering::Less);
    }

    /// Integration tests for `inline_key_fast` covering:
    ///
    /// 1. Monotonic ordering across increasing lengths and lexical variations.
    /// 2. Cross-check against `GenericBinaryArray` comparison to ensure semantic equivalence.
    ///
    /// This also includes a specific test for the “bar” vs. “bar\0” case, demonstrating why
    /// the length field is required even when all inline bytes fit in 12 bytes.
    #[test]
    fn test_inline_key_fast_various_lengths_and_lexical() {
        /// Helper to create a raw u128 value representing an inline ByteView
        /// - `length`: number of meaningful bytes (≤ 12)
        /// - `data`: the actual inline data
        fn make_raw_inline(length: u32, data: &[u8]) -> u128 {
            assert!(length as usize <= 12, "Inline length must be ≤ 12");
            assert!(data.len() == length as usize, "Data must match length");

            let mut raw_bytes = [0u8; 16];
            raw_bytes[0..4].copy_from_slice(&length.to_le_bytes()); // little-endian length
            raw_bytes[4..(4 + data.len())].copy_from_slice(data); // inline data
            u128::from_le_bytes(raw_bytes)
        }

        // Test inputs: include the specific "bar" vs "bar\0" case, plus length and lexical variations
        let test_inputs: Vec<&[u8]> = vec![
            b"a",
            b"aa",
            b"aaa",
            b"aab",
            b"abcd",
            b"abcde",
            b"abcdef",
            b"abcdefg",
            b"abcdefgh",
            b"abcdefghi",
            b"abcdefghij",
            b"abcdefghijk",
            b"abcdefghijkl", // 12 bytes, max inline
            b"bar",
            b"bar\0", // special case to test length tiebreaker
            b"xyy",
            b"xyz",
        ];

        // Monotonic key order: content then length，and cross-check against GenericBinaryArray comparison
        let array: GenericBinaryArray<i32> = GenericBinaryArray::from(
            test_inputs.iter().map(|s| Some(*s)).collect::<Vec<_>>(),
        );

        for i in 0..array.len() - 1 {
            let v1 = array.value(i);
            let v2 = array.value(i + 1);
            // Ensure lexical ordering matches
            assert!(v1 < v2, "Array compare failed: {v1:?} !< {v2:?}");
            // Ensure fast key compare matches
            let key1 = inline_key_fast(make_raw_inline(v1.len() as u32, v1));
            let key2 = inline_key_fast(make_raw_inline(v2.len() as u32, v2));
            assert!(
                key1 < key2,
                "Key compare failed: key({v1:?})=0x{key1:032x} !< key({v2:?})=0x{key2:032x}",
            );
        }
    }
}
