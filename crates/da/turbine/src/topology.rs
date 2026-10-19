use std::collections::HashMap;

#[derive(Clone, Debug, Default)]
pub struct TurbineTopology {
    layers: Vec<Vec<String>>,
    adjacency: HashMap<String, Vec<String>>,
}

impl TurbineTopology {
    pub fn new(layers: Vec<Vec<String>>) -> Self {
        let mut topology = TurbineTopology {
            layers,
            adjacency: HashMap::new(),
        };
        topology.rebuild_adjacency();
        topology
    }

    pub fn layers(&self) -> &[Vec<String>] {
        &self.layers
    }

    pub fn layer(&self, depth: usize) -> Option<&[String]> {
        self.layers.get(depth).map(|layer| layer.as_slice())
    }

    pub fn children(&self, node: &str) -> Vec<String> {
        self.adjacency.get(node).cloned().unwrap_or_else(Vec::new)
    }

    pub fn add_layer(&mut self, layer: Vec<String>) {
        self.layers.push(layer);
        self.rebuild_adjacency();
    }

    #[allow(clippy::manual_div_ceil)]
    fn rebuild_adjacency(&mut self) {
        self.adjacency.clear();
        for idx in 0..self.layers.len().saturating_sub(1) {
            let parents = &self.layers[idx];
            let children = &self.layers[idx + 1];
            if parents.is_empty() {
                continue;
            }

            let stride = (children.len() + parents.len() - 1) / parents.len();
            for (parent_idx, parent) in parents.iter().enumerate() {
                let start = parent_idx * stride;
                let end = ((parent_idx + 1) * stride).min(children.len());
                let slice = if start < end {
                    children[start..end].to_vec()
                } else {
                    Vec::new()
                };
                self.adjacency.insert(parent.clone(), slice);
            }
        }
    }
}
