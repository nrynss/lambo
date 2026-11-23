//! Property tests: [`tie_break_by_key`] against
//! `formal/LamboProofs/Order.lean`.

use std::cmp::Ordering;

use proptest::collection::{hash_set, vec};
use proptest::prelude::*;

use super::*;
use crate::test_util::props::{self, key};

/// A member: its id and its key.
type Member = (NodeId, Option<String>);

/// Up to eight members with distinct ids and random keys, and a random
/// permutation of them.
fn members() -> impl Strategy<Value = (Vec<Member>, Vec<Member>)> {
    hash_set(0u128..64, 1..9)
        .prop_flat_map(|ids| {
            let ids: Vec<u128> = ids.into_iter().collect();
            let n = ids.len();
            (Just(ids), vec(key(), n))
        })
        .prop_flat_map(|(ids, keys)| {
            let items: Vec<Member> = ids
                .into_iter()
                .map(|i| NodeId(uuid::Uuid::from_u128(i)))
                .zip(keys)
                .collect();
            (Just(items.clone()), Just(items).prop_shuffle())
        })
}

fn cmp(a: &Member, b: &Member) -> Ordering {
    tie_break_by_key(a.1.as_deref(), &a.0, b.1.as_deref(), &b.0)
}

/// Lean `Order.tieBreak` as a derived `Ord` key ([`props::spec_tie_key`]).
fn spec_key(m: &Member) -> props::SpecTieKey {
    props::spec_tie_key(m.1.as_deref(), m.0)
}

proptest! {
    #![proptest_config(props::config())]

    /// `Order.tieBreak_lawful`, `Order.tieBreak_separating` and
    /// `Order.sorted_unique` (`formal/LamboProofs/Order.lean`), on random
    /// mixed keyed and keyless members (key a function of the id, as in the
    /// model):
    ///
    /// * `Lawful.swap`: comparing the other way round reverses the answer;
    /// * `Lawful.lt_trans`: `Less` is transitive;
    /// * `Lawful.eq_left` and `Separating`: only a member equals itself;
    /// * `sorted_unique`: sorting any permutation gives the same list;
    /// * the list is sorted exactly as Lean `tieBreak` sorts it
    ///   ([`spec_key`]): keyless last, tie-form key ascending, id ascending.
    #[test]
    fn tie_break_by_key_is_a_strict_total_order((items, shuffled) in members()) {
        for a in &items {
            for b in &items {
                prop_assert_eq!(cmp(a, b), cmp(b, a).reverse(), "swap: {:?} {:?}", a, b);
                if cmp(a, b) == Ordering::Equal {
                    prop_assert_eq!(a.0, b.0, "separating");
                }
                for c in &items {
                    if cmp(a, b) == Ordering::Less && cmp(b, c) == Ordering::Less {
                        prop_assert_eq!(cmp(a, c), Ordering::Less, "lt_trans: {:?} {:?} {:?}", a, b, c);
                    }
                }
            }
        }
        let mut x = items.clone();
        let mut y = shuffled;
        x.sort_by(cmp);
        y.sort_by(cmp);
        prop_assert_eq!(&x, &y);
        let mut z = items;
        z.sort_by_key(spec_key);
        prop_assert_eq!(x, z, "not Lean tieBreak's order");
    }
}
