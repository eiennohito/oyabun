mod chromium;

use super::GroupRule;

pub(crate) fn default_rules() -> Vec<Box<dyn GroupRule>> {
    vec![Box::new(chromium::ChromiumRule)]
}
