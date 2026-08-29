use anyhow::Context as _;

pub fn render_message(template: &str, config: &minijinja::Value) -> anyhow::Result<String> {
    let env = base_env();
    env.render_str(template, minijinja::context! { config })
        .with_context(|| format!("template {template:?}"))
}

fn base_env() -> minijinja::Environment<'static> {
    let mut env = minijinja::Environment::new();
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
    env
}

#[derive(Clone)]
pub struct ArgsTemplate {
    elements: Vec<String>,
    takes_value: bool,
}

impl ArgsTemplate {
    pub(crate) fn new(elements: Vec<String>) -> anyhow::Result<Self> {
        let env = base_env();
        let mut takes_value = false;
        for element in &elements {
            let template = env
                .template_from_str(element)
                .with_context(|| format!("template {element:?}"))?;
            let variables = template.undeclared_variables(false);
            for variable in &variables {
                anyhow::ensure!(
                    variable == "value",
                    "template {element:?} names {{{{ {variable} }}}}; \
                     only {{{{ value }}}} exists",
                );
            }
            takes_value = takes_value || variables.contains("value");
        }
        Ok(Self {
            elements,
            takes_value,
        })
    }

    #[must_use]
    pub const fn takes_value(&self) -> bool {
        self.takes_value
    }

    #[must_use]
    pub fn elements(&self) -> &[String] {
        &self.elements
    }

    pub fn fill(&self, value: &str) -> anyhow::Result<Vec<String>> {
        let env = base_env();
        self.elements
            .iter()
            .map(|element| {
                env.render_str(element, minijinja::context! { value })
                    .with_context(|| format!("template {element:?}"))
            })
            .collect()
    }
}
