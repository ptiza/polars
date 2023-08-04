use smartstring::alias::String as SmartString;

use super::*;

pub(super) fn profile_name(
    s: &dyn PhysicalExpr,
    input_schema: &Schema,
    has_cse: bool,
) -> PolarsResult<SmartString> {
    match (has_cse, s.to_field(input_schema)) {
        (false, Err(e)) => Err(e),
        (true, Err(_)) => Ok(expr_to_leaf_column_names_iter(s.as_expression().unwrap())
            .map(|n| n.as_ref().into())
            .next()
            .unwrap()),
        (_, Ok(fld)) => Ok(fld.name),
    }
}

fn execute_projection_cached_window_fns(
    df: &DataFrame,
    exprs: &[Arc<dyn PhysicalExpr>],
    state: &ExecutionState,
) -> PolarsResult<Vec<Series>> {
    // We partition by normal expression and window expression
    // - the normal expressions can run in parallel
    // - the window expression take more memory and often use the same groupby keys and join tuples
    //   so they are cached and run sequential

    // the partitioning messes with column order, so we also store the idx
    // and use those to restore the original projection order
    #[allow(clippy::type_complexity)]
    // String: partition_name,
    // u32: index,
    let mut windows: Vec<(String, Vec<(u32, Arc<dyn PhysicalExpr>)>)> = vec![];
    let mut other = Vec::with_capacity(exprs.len());

    // first we partition the window function by the values they group over.
    // the groupby values should be cached
    let mut index = 0u32;
    exprs.iter().for_each(|phys| {
        index += 1;
        let e = phys.as_expression().unwrap();

        let mut is_window = false;
        for e in e.into_iter() {
            if let Expr::Window { partition_by, .. } = e {
                let groupby = format!("{:?}", partition_by.as_slice());
                if let Some(tpl) = windows.iter_mut().find(|tpl| tpl.0 == groupby) {
                    tpl.1.push((index, phys.clone()))
                } else {
                    windows.push((groupby, vec![(index, phys.clone())]))
                }
                is_window = true;
                break;
            }
        }
        if !is_window {
            other.push((index, phys))
        }
    });

    let mut selected_columns = POOL.install(|| {
        other
            .par_iter()
            .map(|(idx, expr)| expr.evaluate(df, state).map(|s| (*idx, s)))
            .collect::<PolarsResult<Vec<_>>>()
    })?;

    for partition in windows {
        // clear the cache for every partitioned group
        let mut state = state.split();
        // inform the expression it has window functions.
        state.insert_has_window_function_flag();

        // don't bother caching if we only have a single window function in this partition
        if partition.1.len() == 1 {
            state.remove_cache_window_flag();
        } else {
            state.insert_cache_window_flag();
        }

        for (index, e) in partition.1 {
            if e.as_expression()
                .unwrap()
                .into_iter()
                .filter(|e| matches!(e, Expr::Window { .. }))
                .count()
                == 1
            {
                state.insert_cache_window_flag();
            }
            // caching more than one window expression is a complicated topic for another day
            // see issue #2523
            else {
                state.remove_cache_window_flag();
            }

            let s = e.evaluate(df, &state)?;
            selected_columns.push((index, s));
        }
    }

    selected_columns.sort_unstable_by_key(|tpl| tpl.0);
    let selected_columns = selected_columns.into_iter().map(|tpl| tpl.1).collect();
    Ok(selected_columns)
}

fn run_exprs_par(
    df: &DataFrame,
    exprs: &[Arc<dyn PhysicalExpr>],
    state: &ExecutionState,
) -> PolarsResult<Vec<Series>> {
    POOL.install(|| {
        exprs
            .par_iter()
            .map(|expr| expr.evaluate(df, state))
            .collect()
    })
}

pub(super) fn evaluate_physical_expressions(
    df: &mut DataFrame,
    cse_exprs: &[Arc<dyn PhysicalExpr>],
    exprs: &[Arc<dyn PhysicalExpr>],
    state: &ExecutionState,
    has_windows: bool,
) -> PolarsResult<Vec<Series>> {
    let runner = if has_windows {
        execute_projection_cached_window_fns
    } else {
        run_exprs_par
    };

    let selected_columns = if !cse_exprs.is_empty() {
        let tmp_cols = runner(df, cse_exprs, state)?;
        if has_windows {
            state.clear_window_expr_cache();
        }

        let width = df.width();

        // put the cse expressions at the end
        unsafe {
            df.hstack_mut_unchecked(&tmp_cols);
        }
        let mut result = runner(df, exprs, state)?;
        // restore original df
        unsafe {
            df.get_columns_mut().truncate(width);
        }

        // the replace CSE has a temporary name
        // we don't want this name in the result
        for s in result.iter_mut() {
            rename_cse_tmp_series(s);
        }

        result
    } else {
        runner(df, exprs, state)?
    };

    if has_windows {
        state.clear_window_expr_cache();
    }

    Ok(selected_columns)
}

pub(super) fn check_expand_literals(
    mut selected_columns: Vec<Series>,
    zero_length: bool,
) -> PolarsResult<DataFrame> {
    let first_len = selected_columns[0].len();
    let mut df_height = 0;
    let mut all_equal_len = true;
    {
        let mut names = PlHashSet::with_capacity(selected_columns.len());
        for s in &selected_columns {
            let len = s.len();
            df_height = std::cmp::max(df_height, len);
            if len != first_len {
                all_equal_len = false;
            }
            let name = s.name();
            polars_ensure!(names.insert(name), duplicate = name);
        }
    }
    // If all series are the same length it is ok. If not we can broadcast Series of length one.
    if !all_equal_len {
        selected_columns = selected_columns
            .into_iter()
            .map(|series| {
                Ok(if series.len() == 1 && df_height > 1 {
                    series.new_from_index(0, df_height)
                } else if series.len() == df_height || series.len() == 0 {
                    series
                } else {
                    polars_bail!(
                        ComputeError: "series length {} doesn't match the dataframe height of {}",
                        series.len(), df_height
                    );
                })
            })
            .collect::<PolarsResult<_>>()?
    }

    let df = DataFrame::new_no_checks(selected_columns);

    // a literal could be projected to a zero length dataframe.
    // This prevents a panic.
    let df = if zero_length {
        let min = df.get_columns().iter().map(|s| s.len()).min();
        if min.is_some() {
            df.head(min)
        } else {
            df
        }
    } else {
        df
    };
    Ok(df)
}