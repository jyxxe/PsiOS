CREATE TABLE model_provider_authorities (
    lease TEXT PRIMARY KEY REFERENCES model_inference_leases(lease),
    root_grant TEXT NOT NULL,
    binding TEXT NOT NULL CHECK(json_type(binding)='object')
) STRICT;
INSERT INTO meta(key,value)
    SELECT 'model_provider_authority_epoch',CAST(COALESCE(MAX(seq),0) AS TEXT) FROM versions;
