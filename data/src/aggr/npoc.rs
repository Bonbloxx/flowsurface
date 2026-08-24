use exchange::unit::Price;

/// Find the closest later interval containing each query price.
///
/// The result at index `i` is the smallest `j > i` whose inclusive
/// `[low, high]` interval contains `queries[i]`. Price coordinates remain the
/// exchange's exact fixed-point integers; coordinate compression changes only
/// how the search is performed.
pub(super) fn nearest_future_interval_hits(
    intervals: &[(Price, Price)],
    queries: &[Option<Price>],
) -> Vec<Option<usize>> {
    debug_assert_eq!(intervals.len(), queries.len());

    let mut coordinates = queries.iter().flatten().copied().collect::<Vec<_>>();
    coordinates.sort_unstable();
    coordinates.dedup();

    let mut hits = vec![None; queries.len()];
    if coordinates.is_empty() {
        return hits;
    }

    let mut assignments = RangeAssignments::new(coordinates.len());
    for index in (0..queries.len()).rev() {
        if let Some(query) = queries[index] {
            let coordinate = coordinates
                .binary_search(&query)
                .expect("query coordinates were collected before indexing");
            hits[index] = assignments.get(coordinate);
        }

        let (low, high) = intervals[index];
        if low > high {
            continue;
        }
        let start = coordinates.partition_point(|price| *price < low);
        let end = coordinates.partition_point(|price| *price <= high);
        if start < end {
            assignments.assign(start, end - 1, index);
        }
    }

    hits
}

/// A lazy segment tree supporting inclusive range assignment and point lookup.
/// Processing intervals newest-to-oldest means each newer assignment is
/// overwritten only by a closer future interval.
struct RangeAssignments {
    len: usize,
    nodes: Vec<Option<usize>>,
}

impl RangeAssignments {
    fn new(len: usize) -> Self {
        Self {
            len,
            nodes: vec![None; len.saturating_mul(4).max(1)],
        }
    }

    fn assign(&mut self, start: usize, end: usize, value: usize) {
        self.assign_node(1, 0, self.len - 1, start, end, value);
    }

    fn assign_node(
        &mut self,
        node: usize,
        node_start: usize,
        node_end: usize,
        start: usize,
        end: usize,
        value: usize,
    ) {
        if start <= node_start && node_end <= end {
            self.nodes[node] = Some(value);
            return;
        }

        self.push(node);
        let midpoint = node_start + (node_end - node_start) / 2;
        if start <= midpoint {
            self.assign_node(node * 2, node_start, midpoint, start, end, value);
        }
        if end > midpoint {
            self.assign_node(node * 2 + 1, midpoint + 1, node_end, start, end, value);
        }
    }

    fn get(&self, coordinate: usize) -> Option<usize> {
        self.get_node(1, 0, self.len - 1, coordinate)
    }

    fn get_node(
        &self,
        node: usize,
        node_start: usize,
        node_end: usize,
        coordinate: usize,
    ) -> Option<usize> {
        if let Some(value) = self.nodes[node] {
            return Some(value);
        }
        if node_start == node_end {
            return None;
        }

        let midpoint = node_start + (node_end - node_start) / 2;
        if coordinate <= midpoint {
            self.get_node(node * 2, node_start, midpoint, coordinate)
        } else {
            self.get_node(node * 2 + 1, midpoint + 1, node_end, coordinate)
        }
    }

    fn push(&mut self, node: usize) {
        if let Some(value) = self.nodes[node].take() {
            self.nodes[node * 2] = Some(value);
            self.nodes[node * 2 + 1] = Some(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price(units: i64) -> Price {
        Price::from_units(units)
    }

    fn reference(intervals: &[(Price, Price)], queries: &[Option<Price>]) -> Vec<Option<usize>> {
        queries
            .iter()
            .enumerate()
            .map(|(index, query)| {
                let query = query.as_ref()?;
                intervals
                    .iter()
                    .enumerate()
                    .skip(index + 1)
                    .find_map(|(future, (low, high))| {
                        (*low <= *query && *high >= *query).then_some(future)
                    })
            })
            .collect()
    }

    #[test]
    fn finds_the_nearest_future_interval_at_inclusive_boundaries() {
        let intervals = [
            (price(0), price(1)),
            (price(3), price(8)),
            (price(2), price(5)),
            (price(7), price(9)),
        ];
        let queries = [
            Some(price(5)),
            Some(price(7)),
            Some(price(9)),
            Some(price(8)),
        ];

        assert_eq!(
            nearest_future_interval_hits(&intervals, &queries),
            vec![Some(1), Some(3), Some(3), None]
        );
    }

    #[test]
    fn optimized_lookup_matches_quadratic_reference() {
        let mut state = 0x4d59_5df4_d0f3_3173_u64;
        for len in [1, 2, 3, 31, 127, 511] {
            let mut intervals = Vec::with_capacity(len);
            let mut queries = Vec::with_capacity(len);
            for index in 0..len {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let center = ((state >> 32) % 200) as i64 - 100;
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let width = ((state >> 32) % 20) as i64;
                intervals.push((price(center - width), price(center + width)));
                queries.push((index % 7 != 0).then(|| price(center + (index % 9) as i64 - 4)));
            }

            assert_eq!(
                nearest_future_interval_hits(&intervals, &queries),
                reference(&intervals, &queries)
            );
        }
    }
}
