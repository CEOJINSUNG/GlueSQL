use {
    crate::{
        ast::{ColumnDef, ColumnOption, ColumnOptionDef},
        data::{Key, Row, Value},
        result::Result,
        store::Store,
    },
    im_rc::HashSet,
    serde::Serialize,
    std::{fmt::Debug, rc::Rc},
    thiserror::Error as ThisError,
    utils::Vector,
};

#[derive(ThisError, Debug, PartialEq, Serialize)]
pub enum ValidateError {
    #[error("conflict! storage row has no column on index {0}")]
    ConflictOnStorageColumnIndex(usize),

    #[error("duplicate entry '{}' for unique column '{1}'", String::from(.0))]
    DuplicateEntryOnUniqueField(Value, String),

    #[error("duplicate entry '{0:?}' for primary_key field")]
    DuplicateEntryOnPrimaryKeyField(Key),
}

pub enum ColumnValidation {
    /// `INSERT`
    All(Rc<[ColumnDef]>),
    /// `UPDATE`
    SpecifiedColumns(Rc<[ColumnDef]>, Vec<String>),
}

#[derive(Debug)]
struct UniqueConstraint {
    column_index: usize,
    column_name: String,
    keys: HashSet<Key>,
}

impl UniqueConstraint {
    fn new(column_index: usize, column_name: String) -> Self {
        Self {
            column_index,
            column_name,
            keys: HashSet::new(),
        }
    }

    fn add(self, value: &Value) -> Result<Self> {
        let new_key = self.check(value)?;

        if matches!(new_key, Key::None) {
            return Ok(self);
        }

        let keys = self.keys.update(new_key);

        Ok(Self {
            column_index: self.column_index,
            column_name: self.column_name,
            keys,
        })
    }

    fn check(&self, value: &Value) -> Result<Key> {
        let key = Key::try_from(value)?;

        if !self.keys.contains(&key) {
            Ok(key)
        } else {
            Err(ValidateError::DuplicateEntryOnUniqueField(
                value.clone(),
                self.column_name.to_owned(),
            )
            .into())
        }
    }
}

pub async fn validate_unique(
    storage: &dyn Store,
    table_name: &str,
    column_validation: ColumnValidation,
    row_iter: impl Iterator<Item = &Row> + Clone,
) -> Result<()> {
    enum Columns {
        /// key index
        PrimaryKeyOnly(usize),
        /// `[(key_index, table_name)]`
        All(Vec<(usize, String)>),
    }

    let columns = match &column_validation {
        ColumnValidation::All(column_defs) => {
            let primary_key_index = column_defs
                .iter()
                .enumerate()
                .find(|(_, column_def)| {
                    column_def
                        .options
                        .iter()
                        .any(|ColumnOptionDef { option, .. }| {
                            matches!(option, ColumnOption::Unique { is_primary: true })
                        })
                })
                .map(|(i, _)| i);
            let other_unique_column_def_count = column_defs
                .iter()
                .filter(|column_def| {
                    column_def
                        .options
                        .iter()
                        .any(|ColumnOptionDef { option, .. }| {
                            matches!(option, ColumnOption::Unique { is_primary: false })
                        })
                })
                .count();

            match (primary_key_index, other_unique_column_def_count) {
                (Some(primary_key_index), 0) => Columns::PrimaryKeyOnly(primary_key_index),
                _ => Columns::All(fetch_all_unique_columns(column_defs)),
            }
        }
        ColumnValidation::SpecifiedColumns(column_defs, specified_columns) => Columns::All(
            fetch_specified_unique_columns(column_defs, specified_columns),
        ),
    };

    match columns {
        Columns::PrimaryKeyOnly(primary_key_index) => {
            for primary_key in
                row_iter.filter_map(|row| row.0.get(primary_key_index).map(Key::try_from))
            {
                let key = primary_key?;

                if storage.fetch_data(table_name, &key).await?.is_some() {
                    return Err(ValidateError::DuplicateEntryOnPrimaryKeyField(key).into());
                }
            }

            Ok(())
        }
        Columns::All(columns) => {
            let unique_constraints: Vec<_> = create_unique_constraints(columns, row_iter)?.into();
            if unique_constraints.is_empty() {
                return Ok(());
            }

            let unique_constraints = Rc::new(unique_constraints);
            storage.scan_data(table_name).await?.try_for_each(|result| {
                let (_, row) = result?;
                Rc::clone(&unique_constraints)
                    .iter()
                    .try_for_each(|constraint| {
                        let col_idx = constraint.column_index;
                        let val = row
                            .get_value_by_index(col_idx)
                            .ok_or(ValidateError::ConflictOnStorageColumnIndex(col_idx))?;

                        constraint.check(val)?;

                        Ok(())
                    })
            })
        }
    }
}

fn create_unique_constraints<'a>(
    unique_columns: Vec<(usize, String)>,
    row_iter: impl Iterator<Item = &'a Row> + Clone,
) -> Result<Vector<UniqueConstraint>> {
    unique_columns
        .into_iter()
        .try_fold(Vector::new(), |constraints, col| {
            let (col_idx, col_name) = col;
            let new_constraint = UniqueConstraint::new(col_idx, col_name);
            let new_constraint = row_iter
                .clone()
                .try_fold(new_constraint, |constraint, row| {
                    let val = row
                        .get_value_by_index(col_idx)
                        .ok_or(ValidateError::ConflictOnStorageColumnIndex(col_idx))?;

                    constraint.add(val)
                })?;
            Ok(constraints.push(new_constraint))
        })
}

fn fetch_all_unique_columns(column_defs: &[ColumnDef]) -> Vec<(usize, String)> {
    column_defs
        .iter()
        .enumerate()
        .filter_map(|(i, table_col)| {
            table_col
                .options
                .iter()
                .any(|opt_def| matches!(opt_def.option, ColumnOption::Unique { .. }))
                .then_some((i, table_col.name.to_owned()))
        })
        .collect()
}

fn fetch_specified_unique_columns(
    all_column_defs: &[ColumnDef],
    specified_columns: &[String],
) -> Vec<(usize, String)> {
    all_column_defs
        .iter()
        .enumerate()
        .filter_map(|(i, table_col)| {
            table_col
                .options
                .iter()
                .any(|ColumnOptionDef { option, .. }| match option {
                    ColumnOption::Unique { .. } => specified_columns
                        .iter()
                        .any(|specified_col| specified_col == &table_col.name),
                    _ => false,
                })
                .then_some((i, table_col.name.to_owned()))
        })
        .collect()
}
