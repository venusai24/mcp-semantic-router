use anyhow::Result;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use ordered_float::OrderedFloat;
use serde_json::Value;
use std::sync::{Arc, Mutex, RwLock};

/// Represents a tool stored in our semantic index
#[derive(Debug, Clone)]
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

        Ok(Self {
            model: Arc::new(Mutex::new(model)),
            index: Arc::new(RwLock::new(Vec::new())),
        })
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
        
        // FIX: Added 'mut' here
        let embeddings = tokio::task::spawn_blocking(move || {
            let mut model = model_clone.lock().unwrap(); 
            model.embed(texts_to_embed, None)
        }).await??;

        let mut index_guard = self.index.write().unwrap();
        
        index_guard.retain(|t| t.server_id != server_id);

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
        Ok(())
    }

    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<IndexedTool>> {
        let model_clone = self.model.clone();
        let query_string = query.to_string();

        // FIX: Added 'mut' here
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