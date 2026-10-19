pub fn missing_indices(total: usize, present: &[u32]) -> Vec<u32> {
    let mut present_set = present.to_vec();
    present_set.sort_unstable();
    present_set.dedup();

    (0..total as u32)
        .filter(|idx| !present_set.contains(idx))
        .collect()
}
