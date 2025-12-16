use anyhow::Result;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize}; // Added Serialize/Deserialize
use serde_json::Value;
use std::sync::{Arc, Mutex, RwLock};
use std::fs::File;
use std::io::{BufReader, BufWriter};

const INDEX_FILE: &str = "router_index.bin";

/// Represents a tool stored in our semantic index
// FIX: Added Serialize, Deserialize so we can save to disk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexedTool {
    pub server_id: String,
    pub name: String,
    pub description: String,
    pub full_schema: Value,
    pub embedding: Vec<f32>,
}

pub struct SemanticEngine {
    model: Arc<Mutex<TextEmbedding>>,
    index: Arc<RwLock<Vec<IndexedTool>>>,
}

impl SemanticEngine {
    pub fn new() -> Result<Self> {
        let mut options = InitOptions::default();
        options.model_name = EmbeddingModel::AllMiniLML6V2;
        options.show_download_progress = true;

        let model = TextEmbedding::try_new(options)?;
        
        // 1. Try to load existing index from disk
        let initial_index = Self::load_from_disk().unwrap_or_else(|_| {
            println!("📂 No existing index found. Starting fresh.");
            Vec::new()
        });

        Ok(Self {
            model: Arc::new(Mutex::new(model)),
            index: Arc::new(RwLock::new(initial_index)),
        })
    }

    /// Internal helper to load index
    fn load_from_disk() -> Result<Vec<IndexedTool>> {
        if !std::path::Path::new(INDEX_FILE).exists() {
            return Ok(Vec::new());
        }
        
        let file = File::open(INDEX_FILE)?;
        let reader = BufReader::new(file);
        let tools: Vec<IndexedTool> = bincode::deserialize_from(reader)?;
        
        println!("💾 Loaded {} tools from disk cache.", tools.len());
        Ok(tools)
    }

    /// Internal helper to save index
    fn save_to_disk(tools: &Vec<IndexedTool>) -> Result<()> {
        let file = File::create(INDEX_FILE)?;
        let writer = BufWriter::new(file);
        bincode::serialize_into(writer, tools)?;
        Ok(())
    }

    pub async fn ingest_tools(&self, server_id: String, tools: Vec<Value>) -> Result<()> {
        let mut tools_to_index = Vec::new();
        let mut texts_to_embed = Vec::new();

        for tool in tools {
            let name = tool["name"].as_str().unwrap_or("").to_string();
            let description = tool["description"].as_str().unwrap_or("").to_string();
            
            let text = format!("Name: {} Description: {}", name, description);
            
            texts_to_embed.push(text);
            tools_to_index.push((name, description, tool));
        }

        if tools_to_index.is_empty() {
            return Ok(());
        }

        let model_clone = self.model.clone(); 
        
        // Generate embeddings
        let embeddings = tokio::task::spawn_blocking(move || {
            let mut model = model_clone.lock().unwrap(); 
            model.embed(texts_to_embed, None)
        }).await??;

        {
            let mut index_guard = self.index.write().unwrap();
            
            // Remove old tools for this server
            index_guard.retain(|t| t.server_id != server_id);

            // Add new tools
            for ((name, description, schema), embedding) in tools_to_index.into_iter().zip(embeddings) {
                index_guard.push(IndexedTool {
                    server_id: server_id.clone(),
                    name,
                    description,
                    full_schema: schema,
                    embedding,
                });
            }
            println!("Indexed {} tools for server: {}", index_guard.len(), server_id);

            // 2. Save updated index to disk
            if let Err(e) = Self::save_to_disk(&index_guard) {
                eprintln!("⚠️ Failed to persist index to disk: {}", e);
            } else {
                println!("💾 Index persisted to {}", INDEX_FILE);
            }
        }
        
        Ok(())
    }

    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<IndexedTool>> {
        let model_clone = self.model.clone();
        let query_string = query.to_string();

        let query_embedding = tokio::task::spawn_blocking(move || {
            let mut model = model_clone.lock().unwrap();
            let vec = model.embed(vec![query_string], None)?;
            Ok::<Vec<f32>, anyhow::Error>(vec[0].clone())
        }).await??;

        let index_guard = self.index.read().unwrap();
        
        let mut scored_tools: Vec<(f32, &IndexedTool)> = index_guard.iter().map(|tool| {
            let score = cosine_similarity(&query_embedding, &tool.embedding);
            (score, tool)
        }).collect();

        scored_tools.sort_by(|a, b| 
            OrderedFloat(b.0).cmp(&OrderedFloat(a.0))
        );

        let results = scored_tools.into_iter()
            .take(limit)
            .map(|(_, tool)| tool.clone())
            .collect();

        Ok(results)
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot_product: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    
    dot_product / (norm_a * norm_b)
}