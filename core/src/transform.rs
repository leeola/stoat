//! Data transformation system for node links
//!
//! This module provides a flexible transformation system that can be applied
//! to data flowing between nodes through links. Instead of having dedicated
//! transformation nodes, transformations are applied at the link level.

use crate::{value::Value, Result};

/// Represents a data transformation that can be applied to values flowing through links
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Transformation {
    /// Filter data based on a simple expression
    Filter { expression: String },
    /// Sort data by a field
    Sort { field: String, ascending: bool },
    /// Limit the number of items
    Limit { count: usize },
    /// Chain multiple transformations
    Chain {
        transformations: Vec<Transformation>,
    },
}

impl Transformation {
    /// Apply this transformation to a value
    pub fn apply(&self, value: &Value) -> Result<Value> {
        match self {
            Transformation::Filter { expression } => apply_filter(value, expression),
            Transformation::Sort { field, ascending } => apply_sort(value, field, *ascending),
            Transformation::Limit { count } => apply_limit(value, *count),
            Transformation::Chain { transformations } => {
                let mut result = value.clone();
                for transform in transformations {
                    result = transform.apply(&result)?;
                }
                Ok(result)
            },
        }
    }

    /// Create a filter transformation
    pub fn filter(expression: impl Into<String>) -> Self {
        Self::Filter {
            expression: expression.into(),
        }
    }

    /// Create a sort transformation
    pub fn sort(field: impl Into<String>, ascending: bool) -> Self {
        Self::Sort {
            field: field.into(),
            ascending,
        }
    }

    /// Create a limit transformation
    pub fn limit(count: usize) -> Self {
        Self::Limit { count }
    }

    /// Chain multiple transformations together
    pub fn chain(transformations: Vec<Transformation>) -> Self {
        Self::Chain { transformations }
    }
}

/// Apply a filter transformation to array data
fn apply_filter(value: &Value, expression: &str) -> Result<Value> {
    use crate::value::{Array, Map};

    let Value::Array(Array(rows)) = value else {
        return Err(crate::Error::Generic {
            message: "Filter transformation can only be applied to arrays".to_string(),
        });
    };

    // Parse simple filter: "column=value" or "column!=value"
    let (column, operator, filter_value) = parse_filter_expression(expression)?;

    let filtered_rows: Vec<Value> = rows
        .iter()
        .filter(|row| {
            if let Value::Map(Map(map)) = row {
                if let Some(field_value) = map.get(column) {
                    match operator {
                        FilterOperator::Equal => match_values(field_value, filter_value),
                        FilterOperator::NotEqual => !match_values(field_value, filter_value),
                    }
                } else {
                    false
                }
            } else {
                false
            }
        })
        .cloned()
        .collect();

    Ok(Value::Array(Array(filtered_rows)))
}

/// Apply a sort transformation to array data
fn apply_sort(value: &Value, field: &str, ascending: bool) -> Result<Value> {
    use crate::value::Array;

    let Value::Array(Array(rows)) = value else {
        return Err(crate::Error::Generic {
            message: "Sort transformation can only be applied to arrays".to_string(),
        });
    };

    let mut sorted_rows = rows.clone();
    sorted_rows.sort_by(|a, b| {
        let a_val = extract_sort_value(a, field);
        let b_val = extract_sort_value(b, field);

        let comparison = a_val.cmp(&b_val);
        if ascending {
            comparison
        } else {
            comparison.reverse()
        }
    });

    Ok(Value::Array(Array(sorted_rows)))
}

/// Apply a limit transformation to array data
fn apply_limit(value: &Value, count: usize) -> Result<Value> {
    use crate::value::Array;

    let Value::Array(Array(rows)) = value else {
        return Err(crate::Error::Generic {
            message: "Limit transformation can only be applied to arrays".to_string(),
        });
    };

    let limited_rows: Vec<Value> = rows.iter().take(count).cloned().collect();
    Ok(Value::Array(Array(limited_rows)))
}

#[derive(Debug, Clone, PartialEq)]
enum FilterOperator {
    Equal,
    NotEqual,
}

/// Parse a filter expression into components
fn parse_filter_expression(expression: &str) -> Result<(&str, FilterOperator, &str)> {
    if let Some(pos) = expression.find("!=") {
        let column = expression[..pos].trim();
        let value = expression[pos + 2..].trim();
        Ok((column, FilterOperator::NotEqual, value))
    } else if let Some(pos) = expression.find('=') {
        let column = expression[..pos].trim();
        let value = expression[pos + 1..].trim();
        Ok((column, FilterOperator::Equal, value))
    } else {
        Err(crate::Error::Generic {
            message: format!("Invalid filter expression: {}", expression),
        })
    }
}

/// Check if two values match for filtering
fn match_values(field_value: &Value, filter_value: &str) -> bool {
    match field_value {
        Value::String(s) => s.as_str() == filter_value,
        Value::I64(i) => filter_value.parse::<i64>() == Ok(*i),
        Value::U64(u) => filter_value.parse::<u64>() == Ok(*u),
        // Value::Float(f) => filter_value.parse::<f64>() == Ok(f.0),
        Value::Bool(b) => filter_value.parse::<bool>() == Ok(*b),
        _ => false,
    }
}

/// Extract a value for sorting from a map
fn extract_sort_value(value: &Value, field: &str) -> String {
    use crate::value::Map;

    if let Value::Map(Map(map)) = value {
        if let Some(field_value) = map.get(field) {
            match field_value {
                Value::String(s) => s.to_string(),
                Value::I64(i) => format!("{:020}", i), // Zero-pad for proper string sorting
                Value::U64(u) => format!("{:020}", u),
                // Value::Float(f) => format!("{:020.10}", f.0),
                Value::Bool(b) => {
                    if *b {
                        "true".to_string()
                    } else {
                        "false".to_string()
                    }
                },
                _ => String::new(),
            }
        } else {
            String::new()
        }
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{Array, Map};
    use compact_str::CompactString;

    fn create_test_data() -> Value {
        let mut rows = Vec::new();

        // Row 1: name=Alice, age=25, active=true
        let mut row1 = indexmap::IndexMap::new();
        row1.insert(
            CompactString::from("name"),
            Value::String(CompactString::from("Alice")),
        );
        row1.insert(
            CompactString::from("age"),
            Value::String(CompactString::from("25")),
        );
        row1.insert(
            CompactString::from("active"),
            Value::String(CompactString::from("true")),
        );
        rows.push(Value::Map(Map(row1)));

        // Row 2: name=Bob, age=30, active=false
        let mut row2 = indexmap::IndexMap::new();
        row2.insert(
            CompactString::from("name"),
            Value::String(CompactString::from("Bob")),
        );
        row2.insert(
            CompactString::from("age"),
            Value::String(CompactString::from("30")),
        );
        row2.insert(
            CompactString::from("active"),
            Value::String(CompactString::from("false")),
        );
        rows.push(Value::Map(Map(row2)));

        // Row 3: name=Alice, age=35, active=true
        let mut row3 = indexmap::IndexMap::new();
        row3.insert(
            CompactString::from("name"),
            Value::String(CompactString::from("Alice")),
        );
        row3.insert(
            CompactString::from("age"),
            Value::String(CompactString::from("35")),
        );
        row3.insert(
            CompactString::from("active"),
            Value::String(CompactString::from("true")),
        );
        rows.push(Value::Map(Map(row3)));

        Value::Array(Array(rows))
    }

    #[test]
    fn filter_transformation_equal() {
        let data = create_test_data();
        let transform = Transformation::filter("name=Alice");

        let result = transform.apply(&data).unwrap();

        if let Value::Array(Array(rows)) = result {
            assert_eq!(rows.len(), 2); // Should have 2 Alice rows
        } else {
            panic!("Expected array result");
        }
    }

    #[test]
    fn filter_transformation_not_equal() {
        let data = create_test_data();
        let transform = Transformation::filter("name!=Alice");

        let result = transform.apply(&data).unwrap();

        if let Value::Array(Array(rows)) = result {
            assert_eq!(rows.len(), 1); // Should have 1 Bob row
        } else {
            panic!("Expected array result");
        }
    }

    #[test]
    fn limit_transformation() {
        let data = create_test_data();
        let transform = Transformation::limit(2);

        let result = transform.apply(&data).unwrap();

        if let Value::Array(Array(rows)) = result {
            assert_eq!(rows.len(), 2); // Should limit to 2 rows
        } else {
            panic!("Expected array result");
        }
    }

    #[test]
    fn chain_transformations() {
        let data = create_test_data();
        let transform = Transformation::chain(vec![
            Transformation::filter("name=Alice"),
            Transformation::limit(1),
        ]);

        let result = transform.apply(&data).unwrap();

        if let Value::Array(Array(rows)) = result {
            assert_eq!(rows.len(), 1); // Should have 1 Alice row after filter + limit
        } else {
            panic!("Expected array result");
        }
    }

    #[test]
    fn sort_transformation() {
        let data = create_test_data();
        let transform = Transformation::sort("age", false); // Descending by age

        let result = transform.apply(&data).unwrap();

        if let Value::Array(Array(rows)) = result {
            assert_eq!(rows.len(), 3);
            // First row should be Alice with age 35 (highest age)
            if let Value::Map(Map(map)) = &rows[0] {
                if let Some(Value::String(age)) = map.get("age") {
                    assert_eq!(age.as_str(), "35");
                }
            }
        } else {
            panic!("Expected array result");
        }
    }
}
