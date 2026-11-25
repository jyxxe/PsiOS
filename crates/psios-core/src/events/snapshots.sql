CREATE INDEX versions_origin_seq ON versions(seq) WHERE rev=1;
CREATE INDEX versions_origin_kind ON versions(kind,seq) WHERE rev=1;
CREATE INDEX versions_origin_owner ON versions(owner,seq) WHERE rev=1;
CREATE INDEX versions_origin_owner_kind ON versions(owner,kind,seq) WHERE rev=1;
