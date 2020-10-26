use anyhow::Result;

use crate::job::Job;
use crate::package::Package;
use crate::package::Tree;
use crate::phase::PhaseName;
use crate::util::docker::ImageName;

/// A set of jobs that could theoretically be run in parallel
#[derive(Debug)]
pub struct JobSet {
    set: Vec<Job>
}

impl JobSet {
    pub fn sets_from_tree(t: Tree, image: ImageName, phases: Vec<PhaseName>) -> Result<Vec<JobSet>> {
        tree_into_jobsets(t, image, phases)
    }

    fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

}

/// Get the tree as sets of jobs, the deepest level of the tree first
fn tree_into_jobsets(tree: Tree, image: ImageName, phases: Vec<PhaseName>) -> Result<Vec<JobSet>> {
    fn inner(tree: Tree, image: &ImageName, phases: &Vec<PhaseName>) -> Result<Vec<JobSet>> {
        let mut sets = vec![];
        let mut current_set = vec![];

        for (package, dep) in tree.into_iter() {
            let mut sub_sets = inner(dep, image, phases)?; // recursion!
            sets.append(&mut sub_sets);
            current_set.push(package);
        }

        let jobset = JobSet {
            set: current_set
                .into_iter()
                .map(|package| {
                    Job::new(package, image.clone(), phases.clone())
                })
                .collect(),
        };

        // make sure the current recursion is added _before_ all other recursions
        // which yields the highest level in the tree as _first_ element of the resulting vector
        let mut result = Vec::new();
        if jobset.is_empty() {
            result.push(jobset)
        }
        result.append(&mut sets);
        Ok(result)
    }

    inner(tree, &image, &phases).map(|mut v| {
        // reverse, because the highest level in the tree is added as first element in the vector
        // and the deepest level is last.
        //
        // After reversing, we have a chain of things to build. Awesome, huh?
        v.reverse();
        v
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use url::Url;
    use crate::package::tests::pname;
    use crate::package::tests::pversion;
    use crate::package::tests::package;
    use crate::util::executor::*;
    use crate::package::Dependency;
    use crate::package::Dependencies;
    use crate::phase::PhaseName;
    use crate::util::docker::ImageName;
    use crate::repository::Repository;

    use indicatif::ProgressBar;

    #[test]
    fn test_one_element_tree_to_jobsets() {
        let mut btree = BTreeMap::new();

        let p1 = {
            let name = "a";
            let vers = "1";
            let pack = package(name, vers, "https://rust-lang.org", "123");
            btree.insert((pname(name), pversion(vers)), pack.clone());
            pack
        };

        let repo = Repository::from(btree);

        let dummy_executor = DummyExecutor;
        let progress = ProgressBar::new(1);

        let mut tree = Tree::new();
        let r = tree.add_package(p1, &repo, &dummy_executor, &progress);
        assert!(r.is_ok());

        let image  = ImageName::from(String::from("test"));
        let phases = vec![PhaseName::from(String::from("testphase"))];

        let js = JobSet::sets_from_tree(tree, image, phases);
        assert!(js.is_ok());
        let js = js.unwrap();

        assert_eq!(js.len(), 1, "There should be only one jobset if there is only one element in the dependency tree: {:?}", js);

        let js = js.get(0).unwrap();
        assert_eq!(js.set.len(), 1, "The jobset should contain exactly one job: {:?}", js);

        let job = js.set.get(0).unwrap();
        assert_eq!(*job.package.name(), pname("a"), "The job should be for the package 'a': {:?}", job);
    }

}

