pub struct Infobox {
    pub title: String,
    pub entries: Vec<InfoboxEntry>,
}

pub struct InfoboxEntry {
    pub keys: Vec<String>,
    pub description: String,
}
