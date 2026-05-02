use csv::ReaderBuilder;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs::File;
use std::io::BufReader;
use std::process;

/// Represents a row of data in the CSV file
#[derive(Debug, Deserialize)]
struct DataRow {
    #[allow(dead_code)]
    #[serde(flatten)]
    data: csv::StringMap,
}

/// Statistics for a single column
#[derive(Debug, Serialize)]
struct ColumnStats {
    column_name: String,
    mean: f64,
}

/// JSON output structure
#[derive(Debug, Serialize)]
struct JsonOutput {
    stats: Vec<ColumnStats>,
}

fn main() -> Result<(), Box<dyn Error>> {
    // Stage 1: Read CSV file
    let args: Vec<String> = process::env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: {} <input.csv> <output.json>", args[0]);
        process::exit(1);
    }

    let input_path = &args[1];
    let output_path = &args[2];

    eprintln!("Stage 1: Reading CSV file from '{}'...", input_path);

    // Stage 2: Parse CSV into struct
    eprintln!("Stage 2: Parsing CSV into DataRow struct...");
    let file = File::open(input_path).map_err(|e| {
        eprintln!("Error opening file: {:?}", e);
        e
    })?;
    let reader = BufReader::new(file);
    let mut csv_reader = ReaderBuilder::new()
        .has_headers(true)
        .from_reader(reader);

    // Collect all column values
    let mut column_values: csv::StringMap = csv::StringMap::new();
    
    for (i, result) in csv_reader.records().enumerate() {
        let record = result.map_err(|e| {
            eprintln!("Error at row {}: {:?}", i + 2, e);
            e
        })?;
        
        for (column, value) in record.iter() {
            column_values.entry(column.to_string()).or_insert_with(Vec::new).push(
                value.trim().to_string()
            );
        }
    }

    eprintln!("Stage 3: Computing per-column mean...");

    // Stage 3: Compute mean for each column
    let mut stats: Vec<ColumnStats> = column_values
        .iter()
        .map(|(col_name, values)| {
            let sum: f64 = values
                .iter()
                .filter_map(|s| s.parse::<f64>().ok())
                .sum();
            let count = values
                .iter()
                .filter_map(|s| s.parse::<f64>().ok())
                .count();
            
            if count == 0 {
                eprintln!("Warning: Column '{}' has no valid numeric values, skipping.", col_name);
                return ColumnStats {
                    column_name: col_name.clone(),
                    mean: 0.0,
                };
            }
            
            ColumnStats {
                column_name: col_name.clone(),
                mean: sum / count as f64,
            }
        })
        .collect();

    // Sort by column name for consistent output
    stats.sort_by(|a, b| a.column_name.cmp(&b.column_name));

    // Stage 4: Write means to JSON output
    eprintln!("Stage 4: Writing JSON output to '{}'...", output_path);

    let json_output = JsonOutput { stats };
    let json_string = serde_json::to_string_pretty(&json_output).unwrap_or_else(|e| {
        eprintln!("Error serializing to JSON: {:?}", e);
        panic!(e);
    });

    // Write to file
    let output_file = File::create(output_path).map_err(|e| {
        eprintln!("Error creating output file: {:?}", e);
        e
    })?;
    let mut buffer = BufReader::new(output_file);
    
    use std::io::Write;
    buffer.write_all(json_string.as_bytes())?;
    buffer.write_all(b"\n")?;
    
    eprintln!("Done! Output written to {}", output_path);
    Ok(())
}
