//! Mechanism for searching a better pair iterator than state-of-the-art 2D
//! iteration schemes designed for square lattices, via brute force search.

use crate::{
    cache::{self, CacheEntries, CacheModel},
    FeedIdx,
};
use rand::prelude::*;
use std::{collections::BTreeMap, fmt::Write};

/// Configure the level of debugging features from brute force path search.
///
/// This must be a const because brute force search is a CPU intensive process
/// that cannot afford to be constantly testing run-time variables and examining
/// all the paths that achieve a certain cache cost.
///
/// 0 = Don't log anything.
/// 1 = Log search goals, top-level search progress, and the first path that
///     achieves a new cache cost record.
/// 2 = Search and enumerate all the paths that match a certain cache cost record.
/// 3 = Log every time we take a step on a path.
/// 4 = Log the process of searching for a next step on a path.
///
const BRUTE_FORCE_DEBUG_LEVEL: u8 = 1;

/// Pair of feeds
pub type FeedPair = [FeedIdx; 2];

/// Type for storing paths through the 2D pair space
pub type Path = Vec<FeedPair>;

/// Use brute force to find a path which is better than our best strategy so far
/// according to our cache simulation.
pub fn search_best_path(
    num_feeds: FeedIdx,
    entry_size: usize,
    max_radius: FeedIdx,
    mut best_cost: cache::Cost,
) -> Option<(cache::Cost, Path)> {
    // Let's be reasonable here
    assert!(num_feeds > 1 && entry_size > 0 && max_radius >= 1 && best_cost > 0.0);

    // Set up the cache model
    let cache_model = CacheModel::new(entry_size);
    debug_assert!(
        cache_model.max_l1_entries() >= 3,
        "Cache is unreasonably small"
    );

    // In exhaustive mode, make sure that at least we don't re-discover one of
    // the previously discovered strategies.
    if BRUTE_FORCE_DEBUG_LEVEL >= 2 {
        best_cost -= 1.0;
    }

    // A path should go through every point of the 2D half-square defined by
    // x and y belonging to 0..num_feeds and y >= x. From this, we know exactly
    // how long the best path (assuming it exists) will be.
    let path_length = ((num_feeds as usize) * ((num_feeds as usize) + 1)) / 2;

    // We seed the path search algorithm by enumerating every possible starting
    // point for a path, under the following contraints:
    //
    // - To match the output of other algorithms, we want y >= x.
    // - Starting from a point (x, y) is geometrically equivalent to starting
    //   from the symmetric point (num_points-y, num_points-x), so we don't need
    //   to explore both of these starting points to find the optimal solution.
    //
    let mut partial_paths = PartialPaths::new();
    for start_y in 0..num_feeds {
        for start_x in 0..=start_y.min(num_feeds - start_y - 1) {
            partial_paths.push(PartialPath::new(&cache_model, [start_x, start_y]));
        }
    }

    // Precompute the neighbors of every point of the [x, y] domain
    //
    // The constraints on them being...
    //
    // - Next point should be within max_radius of current [x, y] position
    // - Next point should remain within the iteration domain (no greater
    //   than num_feeds, and y >= x).
    //
    // For each point, we store...
    //
    // - The x coordinate of the first neighbor
    // - For this x coordinate and all subsequent ones, the range of y
    //   coordinates of all neighbors that have this x coordinate.
    //
    // This should achieve the indended goal of moving the neighbor constraint
    // logic out of the hot loop, without generating too much memory traffic
    // associated with reading out neighbor coordinates, nor hiding valuable
    // information about the next_x/next_y iteration pattern from the compiler.
    //
    // We also provide a convenient iteration function that produces the
    // iterator of neighbors associated with a certain point from this storage.
    //
    // TODO: In PartialPath, store a table of all points which a path has not
    //       yet been through in a bit-packed format where every word represents
    //       a sets of packed x's words and the y's are bits.
    //
    //       Abstract away PartialPath's storage so that this table is
    //       automatically kept up to date whenever new points are pushed into
    //       the partial path.
    //
    //       During the neighbor search loop, take every x and y in the
    //       specified range, and test the corresponding bit of the packed
    //       table described above.
    //
    //       This should speed up the compiler work of testing whether a path
    //       has been through a certain point, while using minimal space (64
    //       bits per paths for 8 feeds).
    //
    let mut neighbors = vec![(0, vec![]); num_feeds as usize * num_feeds as usize];
    let linear_idx = |curr_x, curr_y| curr_y as usize * num_feeds as usize + curr_x as usize;
    for curr_x in 0..num_feeds {
        for curr_y in curr_x..num_feeds {
            let next_x_range =
                curr_x.saturating_sub(max_radius)..(curr_x + max_radius + 1).min(num_feeds);
            debug_assert!(next_x_range.end < num_feeds);
            debug_assert!((curr_x as isize - next_x_range.start as isize) < max_radius as isize);
            debug_assert!((next_x_range.end as isize - curr_x as isize) <= max_radius as isize);

            let (first_next_x, next_y_ranges) = &mut neighbors[linear_idx(curr_x, curr_y)];
            *first_next_x = next_x_range.start;

            for next_x in next_x_range {
                let next_y_range = curr_y.saturating_sub(max_radius).max(next_x)
                    ..(curr_y + max_radius + 1).min(num_feeds);
                debug_assert!(next_y_range.end < num_feeds);
                debug_assert!(
                    (curr_y as isize - next_y_range.start as isize) < max_radius as isize
                );
                debug_assert!((next_y_range.end as isize - curr_y as isize) <= max_radius as isize);
                debug_assert!(next_y_range.start >= next_x);

                next_y_ranges.push(next_y_range);
            }
        }
    }
    let neighborhood = |curr_x, curr_y| {
        debug_assert!(curr_y >= curr_x);
        let (first_next_x, ref next_y_ranges) = &neighbors[linear_idx(curr_x, curr_y)];
        next_y_ranges.into_iter().cloned().enumerate().flat_map(
            move |(next_x_offset, next_y_range)| {
                next_y_range.map(move |next_y| [first_next_x + next_x_offset as u8, next_y])
            },
        )
    };

    // Next we iterate as long as we have incomplete paths by taking the most
    // promising path so far, considering all the next steps that can be taken
    // on that path, and pushing any further incomplete path that this creates
    // into our list of next actions.
    let mut best_path = Path::new();
    let mut rng = rand::thread_rng();
    while let Some(partial_path) = partial_paths.pop(&mut rng) {
        // Indicate which partial path was chosen
        if BRUTE_FORCE_DEBUG_LEVEL >= 3 {
            let mut path_display = String::new();
            for step in partial_path.iter_rev() {
                write!(path_display, "{:?} -> ", step).unwrap();
            }
            path_display.push_str("END");
            println!(
                "    - Currently on partial path {} with cache cost {}",
                path_display,
                partial_path.cost_so_far()
            );
        }

        // Ignore that path if we found another solution which is so good that
        // it's not worth exploring anymore.
        if partial_path.cost_so_far() > best_cost
            || ((BRUTE_FORCE_DEBUG_LEVEL < 2) && (partial_path.cost_so_far() == best_cost))
        {
            if BRUTE_FORCE_DEBUG_LEVEL >= 4 {
                println!(
                    "      * That exceeds cache cost goal with only {}/{} steps, ignore it.",
                    partial_path.len(),
                    path_length
                );
            }
            continue;
        }

        // Enumerate all possible next points, the constraints on them being...
        // - Next point should not be any point we've previously been through
        // - The total path cache cost is not allowed to go above the best path
        //   cache cost that we've observed so far (otherwise that path is less
        //   interesting than the best path).
        let &[curr_x, curr_y] = partial_path.last_step();
        for next_step in neighborhood(curr_x, curr_y) {
            // Log which neighbor we're looking at in verbose mode
            if BRUTE_FORCE_DEBUG_LEVEL >= 4 {
                println!("      * Trying {:?}...", next_step);
            }

            // Have we been there before ?
            //
            // TODO: This happens to be a performance bottleneck in profiles,
            //       speed it up via the above strategy.
            //
            if partial_path.contains(&next_step) {
                if BRUTE_FORCE_DEBUG_LEVEL >= 4 {
                    println!("      * That's going circles, forget it.");
                }
                continue;
            }

            // Is it worthwhile to go there?
            //
            // TODO: We could consider introducing a stricter cutoff here,
            //       based on the idea that if your partial cache cost is
            //       already X and you have still N steps left to perform,
            //       you're unlikely to beat the best cost.
            //
            //       But that's hard to do due to how chaotically the cache
            //       performs, with most cache misses being at the end of
            //       the curve.
            //
            //       Maybe we could at least track how well our best curve
            //       so far performed at each step, and have a quality
            //       cutoff based on that + a tolerance.
            //
            //       We could then have the search loop start with a fast
            //       low-tolerance search, and resume with a slower
            //       high-tolerance search, ultimately getting to the point
            //       where we can search with infinite tolerance if we truly
            //       want the best of the best curves.
            //
            //       (note: for pairwise iteration that fits in L2 cache, a
            //       tolerance of 2 is an infinite tolerance).
            //
            //       This requires a way to propagate the "best cost at every
            //       step" to the caller, instead of just the the best cost at
            //       the last step, which anyway would be useful once we get to
            //       searching at multiple radii.
            //
            // TODO: Also, we should introduce a sort of undo mechanism (e.g.
            //       an accessor that tells the cache position of a variable and
            //       a mutator that allows us to reset it) in order to delay
            //       memory allocation until the point where we're sure that we
            //       do need to do the cloning.
            //
            let (next_cost, next_entries) =
                partial_path.evaluate_next_step(&cache_model, &next_step);
            if next_cost > best_cost || ((BRUTE_FORCE_DEBUG_LEVEL < 2) && (next_cost == best_cost))
            {
                if BRUTE_FORCE_DEBUG_LEVEL >= 4 {
                    println!(
                        "      * That exceeds cache cost goal with only {}/{} steps, ignore it.",
                        partial_path.len() + 1,
                        path_length
                    );
                }
                continue;
            }

            // Are we finished ?
            let next_path_len = partial_path.len() + 1;
            if next_path_len == path_length {
                if next_cost < best_cost {
                    best_path = partial_path.finish_path(next_step);
                    best_cost = next_cost;
                    if BRUTE_FORCE_DEBUG_LEVEL >= 1 {
                        println!(
                            "  * Reached new cache cost record {} with path {:?}",
                            best_cost, best_path
                        );
                    }
                } else {
                    debug_assert_eq!(next_cost, best_cost);
                    if BRUTE_FORCE_DEBUG_LEVEL >= 2 {
                        println!(
                            "  * Found a path that matches current cache cost constraint: {:?}",
                            partial_path.finish_path(next_step),
                        );
                    }
                }
                continue;
            }

            // Otherwise, schedule searching further down this path
            if BRUTE_FORCE_DEBUG_LEVEL >= 4 {
                println!("      * That seems reasonable, we'll explore that path further...");
            }
            partial_paths.push(partial_path.commit_next_step(next_step, next_cost, next_entries));
        }
        if BRUTE_FORCE_DEBUG_LEVEL >= 3 {
            println!("    - Done exploring possibilities from current path");
        }
    }

    // Return the optimal path, if any, along with its cache cost
    if best_path.is_empty() {
        None
    } else {
        Some((best_cost, best_path))
    }
}

// The amount of possible paths is ridiculously high (of the order of the
// factorial of path_length), so it's extremely important to...
//
// - Finish exploring paths reasonably quickly, to free up RAM and update
//   the "best cost" figure of merit, which in turn allow us to...
// - Prune paths as soon as it becomes clear that they won't beat the
//   current best cost.
// - Explore promising paths first, and make sure we explore large areas of the
//   path space quickly instead of perpetually staying in the same region of the
//   space of possible paths like basic depth-first search would have us do.
//
// To help us with these goals, we store information about the paths which
// we are in the process of exploring in a data structure which is allows
// priorizing the most promising tracks over others.
//
struct PartialPath {
    // TODO: Use a singly linked list of Arc'd feed pairs as path storage in
    //       order to limit storage use and speed up copies.
    //
    //       Yes, readout will be super slow, but that should be a very rare
    //       operation (it only needs to be performed when a path has been fully
    //       explored without being pruned due to excessive cache cast).
    //
    path: Path,
    // TODO: Add a fast index of points that we've been through
    cache_entries: CacheEntries,
    cost_so_far: cache::Cost,
}
//
type RoundedPriority = usize;
//
impl PartialPath {
    /// Start a path
    pub fn new(cache_model: &CacheModel, start: FeedPair) -> Self {
        let path = vec![start];
        let mut cache_entries = cache_model.start_simulation();
        for &feed in start.iter() {
            debug_assert_eq!(cache_model.simulate_access(&mut cache_entries, feed), 0.0);
        }
        Self {
            path,
            cache_entries,
            cost_so_far: 0.0,
        }
    }

    /// Tell how long the path is
    pub fn len(&self) -> usize {
        self.path.len()
    }

    /// Get the last path entry
    pub fn last_step(&self) -> &FeedPair {
        self.path.last().unwrap()
    }

    /// Iterate over the path in reverse step order
    ///
    /// This operation may be slow, and is only intended for debug output.
    ///
    pub fn iter_rev(&self) -> impl Iterator<Item = &FeedPair> {
        self.path.iter().rev()
    }

    /// Tell whether a path contains a certain feed pair
    pub fn contains(&self, pair: &FeedPair) -> bool {
        self.path
            .iter()
            .find(|&prev_pair| prev_pair == pair)
            .is_some()
    }

    /// Get the accumulated cache cost of following this path so far
    pub fn cost_so_far(&self) -> cache::Cost {
        self.cost_so_far
    }

    /// Given an extra feed pair, tell what the accumulated cache cost would
    /// become if the path was completed by this pair, and what the cache
    /// entries would then be.
    //
    // FIXME: Don't compute or return the new cache entries, instead create a
    //        mechanism for temporary cache operations that can be reverted.
    //
    pub fn evaluate_next_step(
        &self,
        cache_model: &CacheModel,
        next_step: &FeedPair,
    ) -> (cache::Cost, CacheEntries) {
        let mut next_cache = self.cache_entries.clone();
        let next_cost = self.cost_so_far
            + next_step
                .iter()
                .map(|&feed| cache_model.simulate_access(&mut next_cache, feed))
                .sum::<f32>();
        (next_cost, next_cache)
    }

    /// Create a new partial path which follows all the steps from this one,
    /// plus an extra step for which the new cache cost and cache entries are
    /// provided.
    //
    // FIXME: Don't require the new cache cost and entries, rework the code so
    //        that evaluate_next_step already has done the necessary work.
    //
    pub fn commit_next_step(
        &self,
        next_step: FeedPair,
        next_cost: cache::Cost,
        next_entries: CacheEntries,
    ) -> Self {
        let mut next_path = self.path.clone();
        next_path.push(next_step);
        Self {
            path: next_path,
            cache_entries: next_entries,
            cost_so_far: next_cost,
        }
    }

    /// Finish this path with a last step
    pub fn finish_path(&self, last_step: FeedPair) -> Path {
        let mut final_path = self.path.clone();
        final_path.push(last_step);
        final_path
    }
}

#[derive(Default)]
struct PartialPaths {
    storage: BTreeMap<RoundedPriority, Vec<PartialPath>>,
}
//
impl PartialPaths {
    /// Create the collection
    pub fn new() -> Self {
        Self::default()
    }

    /// Prioritize a certain path wrt others, higher is more important
    pub fn priorize(path: &PartialPath) -> RoundedPriority {
        // Increasing path length weight means that the highest priority is
        // put on seeing paths through the end (which allows discarding
        // them), decreasing means that the highest priority is put on
        // following through the paths that are most promizing in terms of
        // cache cost (which tends to favor a more breadth-first approach as
        // the first curve points are free of cache costs).
        (1.3 * path.len() as f32 - path.cost_so_far()).round() as _
    }

    /// Record a new partial path
    pub fn push(&mut self, path: PartialPath) {
        let same_priority_paths = self.storage.entry(Self::priorize(&path)).or_default();
        same_priority_paths.push(path);
    }

    /// Extract one of the highest-priority paths
    pub fn pop(&mut self, mut rng: impl Rng) -> Option<PartialPath> {
        let highest_priority_paths = self.storage.values_mut().rev().next()?;
        debug_assert!(!highest_priority_paths.is_empty());
        let path_idx = rng.gen_range(0..highest_priority_paths.len());
        let path = highest_priority_paths.remove(path_idx);
        if highest_priority_paths.is_empty() {
            self.storage.remove(&Self::priorize(&path));
        }
        Some(path)
    }
}
