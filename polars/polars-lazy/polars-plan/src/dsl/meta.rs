use super::*;
use crate::dsl::selector::Selector;
use crate::logical_plan::projection::is_regex_projection;

/// Specialized expressions for Categorical dtypes.
pub struct MetaNameSpace(pub(crate) Expr);

impl MetaNameSpace {
    /// Pop latest expression and return the input(s) of the popped expression.
    pub fn pop(self) -> Vec<Expr> {
        let mut arena = Arena::with_capacity(8);
        let node = to_aexpr(self.0, &mut arena);
        let ae = arena.get(node);
        let mut inputs = Vec::with_capacity(2);
        ae.nodes(&mut inputs);
        inputs
            .iter()
            .map(|node| node_to_expr(*node, &arena))
            .collect()
    }

    /// Get the root column names.
    pub fn root_names(&self) -> Vec<Arc<str>> {
        expr_to_leaf_column_names(&self.0)
    }
    /// A projection that only takes a column or a column + alias.
    pub fn is_simple_projection(&self) -> bool {
        let mut arena = Arena::with_capacity(8);
        let node = to_aexpr(self.0.clone(), &mut arena);
        aexpr_is_simple_projection(node, &arena)
    }

    /// Get the output name of this expression.
    pub fn output_name(&self) -> PolarsResult<Arc<str>> {
        expr_output_name(&self.0)
    }

    /// Undo any renaming operation like `alias`, `keep_name`.
    pub fn undo_aliases(mut self) -> Expr {
        self.0.mutate().apply(|e| match e {
            Expr::Alias(input, _)
            | Expr::KeepName(input)
            | Expr::RenameAlias { expr: input, .. } => {
                // remove this node
                *e = *input.clone();

                // continue iteration
                true
            }
            // continue iteration
            _ => true,
        });

        self.0
    }

    pub fn has_multiple_outputs(&self) -> bool {
        self.0.into_iter().any(|e| match e {
            Expr::Selector(_) | Expr::Wildcard | Expr::Columns(_) | Expr::DtypeColumn(_) => true,
            Expr::Column(name) => is_regex_projection(name),
            _ => false,
        })
    }

    pub fn is_regex_projection(&self) -> bool {
        self.0.into_iter().any(|e| match e {
            Expr::Column(name) => is_regex_projection(name),
            _ => false,
        })
    }

    pub fn _selector_add(self, other: Expr) -> PolarsResult<Expr> {
        if let Expr::Selector(mut s) = self.0 {
            if let Expr::Selector(s_other) = other {
                s = &s + &s_other;
            } else {
                s.add.push(other);
            }
            Ok(Expr::Selector(s))
        } else {
            polars_bail!(ComputeError: "expected selector, got {}", self.0)
        }
    }

    pub fn _selector_sub(self, other: Expr) -> PolarsResult<Expr> {
        if let Expr::Selector(mut s) = self.0 {
            if let Expr::Selector(s_other) = other {
                s = &s - &s_other;
            } else {
                s.subtract.push(other);
            }
            Ok(Expr::Selector(s))
        } else {
            polars_bail!(ComputeError: "expected selector, got {}", self.0)
        }
    }

    pub fn _into_selector(self) -> PolarsResult<Expr> {
        polars_ensure!(!matches!(self.0, Expr::Selector(_)), ComputeError: "nested selectors not allowed");
        Ok(Expr::Selector(Selector::new(self.0)))
    }
}
