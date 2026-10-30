//! Finding synthesis: deterministic pre-dedup clustering followed by the
//! synthesizer session.
//!
//! Pre-dedup (FRG-25, proposal §8.8) clusters producer findings from
//! the three lens sessions on (same file, overlapping line range, same
//! category). Clustering annotates; it deletes nothing: every member
//! reaches the synthesizer tagged with its cluster ID and membership,
//! so a clustering bug can only ever produce an unclustered union —
//! always safe synthesizer input. The synthesizer session itself lands
//! in FRG-26.

use crate::outputs::Finding;

/// One producer finding tagged with its pre-dedup cluster.
///
/// `cluster_size` is the membership count of the cluster (1 for a
/// singleton): the synthesizer sees the full annotated array, so the
/// ID plus the size is everything needed to reference fellow members
/// and to cite the cluster in decision-log entries (FRG-26).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClusteredFinding {
    #[serde(flatten)]
    pub finding: Finding,
    pub cluster_id: String,
    pub cluster_size: usize,
}

/// Cluster producer findings without dropping or reordering any of
/// them.
///
/// Two findings share a cluster when their `file` strings are equal,
/// their `category` strings are equal, and their inclusive
/// `[line_start, line_end]` ranges overlap. Overlap is transitively
/// closed (connected components): if A overlaps B and B overlaps C,
/// all three share one cluster. Ranges touching at a shared line
/// overlap; adjacent-but-disjoint ranges do not. File and category
/// comparison is exact string equality — no path normalization, no
/// case folding — by design: clustering must be boringly
/// deterministic, never clever.
///
/// Output preserves input order, so the emitted multiset of findings
/// always equals the input multiset. Cluster IDs (`cluster-0001`,
/// `cluster-0002`, …) are assigned in order of first appearance in the
/// input: stable across reruns with no timestamps or randomness.
pub fn cluster_findings(findings: &[Finding]) -> Vec<ClusteredFinding> {
    let mut parent: Vec<usize> = (0..findings.len()).collect();
    for left in 0..findings.len() {
        for right in (left + 1)..findings.len() {
            if shares_cluster(&findings[left], &findings[right]) {
                union(&mut parent, left, right);
            }
        }
    }

    let mut cluster_index: Vec<Option<usize>> = vec![None; findings.len()];
    let mut cluster_sizes: Vec<usize> = Vec::new();
    // Resolve roots first so path compression cannot reorder ID
    // assignment: IDs follow first appearance in input order.
    let mut roots: Vec<usize> = Vec::with_capacity(findings.len());
    for position in 0..findings.len() {
        roots.push(find(&mut parent, position));
    }
    for &root in &roots {
        match cluster_index[root] {
            Some(index) => cluster_sizes[index] += 1,
            None => {
                cluster_index[root] = Some(cluster_sizes.len());
                cluster_sizes.push(1);
            }
        }
    }

    roots
        .into_iter()
        .zip(findings.iter().cloned())
        .map(|(root, finding)| {
            let index = cluster_index[root].expect("every root is assigned an index");
            ClusteredFinding {
                finding,
                cluster_id: format!("cluster-{:04}", index + 1),
                cluster_size: cluster_sizes[index],
            }
        })
        .collect()
}

/// Same file, same category, overlapping inclusive line range.
fn shares_cluster(left: &Finding, right: &Finding) -> bool {
    left.file == right.file
        && left.category == right.category
        && left.line_start <= right.line_end
        && right.line_start <= left.line_end
}

fn find(parent: &mut [usize], node: usize) -> usize {
    let mut root = node;
    while parent[root] != root {
        root = parent[root];
    }
    // Path compression: keep later lookups flat.
    let mut cursor = node;
    while parent[cursor] != root {
        let next = parent[cursor];
        parent[cursor] = root;
        cursor = next;
    }
    root
}

fn union(parent: &mut [usize], left: usize, right: usize) {
    let left_root = find(parent, left);
    let right_root = find(parent, right);
    if left_root != right_root {
        parent[left_root.min(right_root)] = left_root.max(right_root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outputs::Severity;

    fn finding(file: &str, start: u64, end: u64, category: &str, title: &str) -> Finding {
        Finding {
            file: file.to_string(),
            line_start: start,
            line_end: end,
            severity: Severity::Medium,
            category: category.to_string(),
            title: title.to_string(),
            description: "description".to_string(),
            evidence: "evidence".to_string(),
            fix_focus_areas: Vec::new(),
            recommended_fix: "fix".to_string(),
            citations: None,
        }
    }

    fn cluster_ids(clustered: &[ClusteredFinding]) -> Vec<&str> {
        clustered
            .iter()
            .map(|member| member.cluster_id.as_str())
            .collect()
    }

    /// The annotation-only invariant: stripping the tags must return
    /// exactly the input multiset, in order.
    fn assert_annotation_only(input: &[Finding], clustered: &[ClusteredFinding]) {
        let stripped: Vec<Finding> = clustered
            .iter()
            .map(|member| member.finding.clone())
            .collect();
        assert_eq!(stripped, input);
        for member in clustered {
            let expected_size = clustered
                .iter()
                .filter(|other| other.cluster_id == member.cluster_id)
                .count();
            assert_eq!(member.cluster_size, expected_size);
        }
    }

    #[test]
    fn exact_duplicates_share_one_cluster() {
        let input = vec![
            finding("src/parse.rs", 10, 20, "Correctness", "first report"),
            finding("src/parse.rs", 10, 20, "Correctness", "second report"),
        ];

        let clustered = cluster_findings(&input);

        assert_eq!(
            cluster_ids(&clustered),
            vec!["cluster-0001", "cluster-0001"]
        );
        assert!(clustered.iter().all(|member| member.cluster_size == 2));
        assert_annotation_only(&input, &clustered);
    }

    #[test]
    fn overlapping_range_with_same_category_clusters() {
        let input = vec![
            finding("src/parse.rs", 10, 20, "Correctness", "wide"),
            finding("src/parse.rs", 18, 30, "Correctness", "overlapping"),
        ];

        let clustered = cluster_findings(&input);

        assert_eq!(
            cluster_ids(&clustered),
            vec!["cluster-0001", "cluster-0001"]
        );
        assert_annotation_only(&input, &clustered);
    }

    #[test]
    fn touching_at_a_shared_line_overlaps() {
        let input = vec![
            finding("src/parse.rs", 10, 20, "Correctness", "left"),
            finding("src/parse.rs", 20, 30, "Correctness", "shares line 20"),
        ];

        let clustered = cluster_findings(&input);

        assert_eq!(
            cluster_ids(&clustered),
            vec!["cluster-0001", "cluster-0001"]
        );
        assert_annotation_only(&input, &clustered);
    }

    #[test]
    fn same_file_and_range_with_different_category_does_not_cluster() {
        let input = vec![
            finding("src/parse.rs", 10, 20, "Correctness", "correctness"),
            finding("src/parse.rs", 10, 20, "Structure", "structure"),
        ];

        let clustered = cluster_findings(&input);

        assert_eq!(
            cluster_ids(&clustered),
            vec!["cluster-0001", "cluster-0002"]
        );
        assert!(clustered.iter().all(|member| member.cluster_size == 1));
        assert_annotation_only(&input, &clustered);
    }

    #[test]
    fn adjacent_but_non_overlapping_ranges_do_not_cluster() {
        let input = vec![
            finding("src/parse.rs", 10, 20, "Correctness", "left"),
            finding("src/parse.rs", 21, 30, "Correctness", "right"),
        ];

        let clustered = cluster_findings(&input);

        assert_eq!(
            cluster_ids(&clustered),
            vec!["cluster-0001", "cluster-0002"]
        );
        assert_annotation_only(&input, &clustered);
    }

    #[test]
    fn different_files_do_not_cluster() {
        let input = vec![
            finding("src/a.rs", 10, 20, "Correctness", "in a"),
            finding("src/b.rs", 10, 20, "Correctness", "in b"),
        ];

        let clustered = cluster_findings(&input);

        assert_eq!(
            cluster_ids(&clustered),
            vec!["cluster-0001", "cluster-0002"]
        );
        assert_annotation_only(&input, &clustered);
    }

    #[test]
    fn transitive_overlap_forms_one_cluster() {
        let input = vec![
            finding("src/parse.rs", 10, 15, "Correctness", "head"),
            finding("src/parse.rs", 14, 25, "Correctness", "bridge"),
            finding("src/parse.rs", 24, 30, "Correctness", "tail"),
        ];

        let clustered = cluster_findings(&input);

        assert_eq!(
            cluster_ids(&clustered),
            vec!["cluster-0001", "cluster-0001", "cluster-0001"]
        );
        assert!(clustered.iter().all(|member| member.cluster_size == 3));
        assert_annotation_only(&input, &clustered);
    }

    #[test]
    fn cluster_ids_follow_first_appearance_and_sizes_agree() {
        let input = vec![
            finding("src/a.rs", 1, 5, "Correctness", "a1"),
            finding("src/b.rs", 1, 5, "Correctness", "b1"),
            finding("src/a.rs", 3, 8, "Correctness", "a2 joins a1"),
            finding("src/b.rs", 2, 4, "Structure", "b2 alone"),
        ];

        let clustered = cluster_findings(&input);

        assert_eq!(
            cluster_ids(&clustered),
            vec![
                "cluster-0001",
                "cluster-0002",
                "cluster-0001",
                "cluster-0003"
            ]
        );
        let sizes: Vec<usize> = clustered.iter().map(|member| member.cluster_size).collect();
        assert_eq!(sizes, vec![2, 1, 2, 1]);
        assert_annotation_only(&input, &clustered);
    }

    #[test]
    fn empty_and_singleton_inputs_are_unchanged_but_tagged() {
        let empty: Vec<Finding> = Vec::new();
        assert_eq!(cluster_findings(&empty), Vec::new());

        let single = vec![finding("src/a.rs", 1, 5, "Correctness", "alone")];
        let clustered = cluster_findings(&single);
        assert_eq!(cluster_ids(&clustered), vec!["cluster-0001"]);
        assert_eq!(clustered[0].cluster_size, 1);
        assert_annotation_only(&single, &clustered);
    }

    #[test]
    fn clustering_is_deterministic_across_runs() {
        let input = vec![
            finding("src/b.rs", 40, 50, "Structure", "s1"),
            finding("src/a.rs", 10, 20, "Correctness", "c1"),
            finding("src/a.rs", 15, 25, "Correctness", "c2"),
            finding("src/b.rs", 45, 60, "Structure", "s2"),
            finding("src/a.rs", 100, 110, "Contract", "k1"),
        ];

        let first = cluster_findings(&input);
        let second = cluster_findings(&input);

        assert_eq!(first, second);
        assert_eq!(
            cluster_ids(&first),
            vec![
                "cluster-0001",
                "cluster-0002",
                "cluster-0002",
                "cluster-0001",
                "cluster-0003"
            ]
        );
        assert_annotation_only(&input, &first);
    }
}
