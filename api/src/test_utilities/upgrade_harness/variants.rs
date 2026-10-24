//! Generating the optional-field variations a seed spreads its objects over

use std::collections::BTreeSet;

use super::types::{VariantSet, Variation};

/// The most fields whose shapes are combined exhaustively
const EXHAUSTIVE_FIELDS: usize = 3;

impl VariantSet {
    /// Every variation a seed covers
    ///
    /// Up to three fields get every combination of their shapes. More fields get pairwise
    /// coverage: a small set of variations in which every pair of two fields' shapes appears
    /// together at least once, which finds the failures that need two fields at once without the
    /// product's size. The order is deterministic, so a failing object has the same number in
    /// every run.
    #[must_use]
    pub fn variations(&self) -> Vec<Variation> {
        // no fields or no shapes leave nothing to vary
        if self.fields.is_empty() {
            return vec![Variation::default()];
        }
        if self.shapes.is_empty() {
            return Vec::new();
        }
        // a few fields are combined exhaustively, and many fields pairwise
        let rows = if self.fields.len() <= EXHAUSTIVE_FIELDS {
            self.product()
        } else {
            self.pairwise()
        };
        // name each field's shape in every row
        rows.into_iter()
            .map(|row| Variation {
                shapes: self
                    .fields
                    .iter()
                    .zip(row)
                    .map(|(field, shape)| (*field, self.shapes[shape]))
                    .collect(),
            })
            .collect()
    }

    /// Every combination of shapes, as one shape index per field
    fn product(&self) -> Vec<Vec<usize>> {
        // count in base `shapes` with the first field the most significant digit
        let base = self.shapes.len();
        let total = base.pow(u32::try_from(self.fields.len()).unwrap_or(u32::MAX));
        (0..total)
            .map(|mut number| {
                let mut row = vec![0; self.fields.len()];
                for digit in row.iter_mut().rev() {
                    *digit = number % base;
                    number /= base;
                }
                row
            })
            .collect()
    }

    /// A set of rows covering every pair of two fields' shapes, as one shape index per field
    fn pairwise(&self) -> Vec<Vec<usize>> {
        // list every pair of shapes of every two fields that still has to appear
        let (fields, shapes) = (self.fields.len(), self.shapes.len());
        let mut uncovered = BTreeSet::new();
        for first in 0..fields {
            for second in first + 1..fields {
                for a in 0..shapes {
                    for b in 0..shapes {
                        uncovered.insert((first, second, a, b));
                    }
                }
            }
        }
        // build one row at a time around the first pair left, until none is
        let mut rows = Vec::new();
        while let Some(&(first, second, a, b)) = uncovered.iter().next() {
            let mut row: Vec<Option<usize>> = vec![None; fields];
            row[first] = Some(a);
            row[second] = Some(b);
            // give every other field the shape that covers the most pairs with those set
            for field in 0..fields {
                if row[field].is_some() {
                    continue;
                }
                let gain = |shape: usize| {
                    row.iter()
                        .enumerate()
                        .filter_map(|(other, set)| set.map(|set| (other, set)))
                        .filter(|(other, set)| {
                            let (low, high, x, y) = if *other < field {
                                (*other, field, *set, shape)
                            } else {
                                (field, *other, shape, *set)
                            };
                            uncovered.contains(&(low, high, x, y))
                        })
                        .count()
                };
                // the lowest shape wins a tie, which keeps the order deterministic
                let best = (0..shapes)
                    .max_by_key(|shape| (gain(*shape), std::cmp::Reverse(*shape)))
                    .unwrap_or(0);
                row[field] = Some(best);
            }
            let row: Vec<usize> = row.into_iter().map(|shape| shape.unwrap_or(0)).collect();
            // the row covers its own pairs
            for first in 0..fields {
                for second in first + 1..fields {
                    uncovered.remove(&(first, second, row[first], row[second]));
                }
            }
            rows.push(row);
        }
        rows
    }
}
