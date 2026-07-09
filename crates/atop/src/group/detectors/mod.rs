mod cgroup;
mod chromium;
mod container;
mod flatpak;
mod runtime;

use super::GroupRule;

pub(crate) fn default_rules() -> Vec<Box<dyn GroupRule>> {
    vec![
        Box::new(flatpak::FlatpakRule),
        Box::new(cgroup::CgroupRule::default()),
        Box::new(chromium::ChromiumRule::default()),
        Box::new(runtime::RuntimePoolRule::default()),
        Box::new(container::ContainerShimRule),
    ]
}
