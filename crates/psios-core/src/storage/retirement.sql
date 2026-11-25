CREATE TABLE storage_reclamations (
    retirement TEXT PRIMARY KEY,
    ready_after TEXT NOT NULL
) STRICT;
CREATE INDEX storage_reclamations_due ON storage_reclamations(ready_after, retirement);
