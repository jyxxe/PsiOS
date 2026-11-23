-- DDL, resource changes and security-generation updates share the witnessed transaction.
CREATE TABLE dispatch_fairness(principal TEXT PRIMARY KEY, turn INTEGER NOT NULL) STRICT;
CREATE TABLE model_budgets(budget TEXT PRIMARY KEY REFERENCES budgets(id), maximum INTEGER NOT NULL,
 used INTEGER NOT NULL DEFAULT 0, CHECK(used>=0 AND used<=maximum)) STRICT;
CREATE TABLE session_turns(session TEXT NOT NULL, turn INTEGER NOT NULL, lease TEXT NOT NULL UNIQUE,
 claim_key TEXT NOT NULL, expires TEXT NOT NULL, task TEXT UNIQUE, decision_digest TEXT, receipt TEXT,
 PRIMARY KEY(session,turn), UNIQUE(session,claim_key)) STRICT;
CREATE INDEX session_turn_task ON session_turns(task) WHERE task IS NOT NULL;
CREATE INDEX versions_owner_history ON versions(owner,kind,id,rev);
PRAGMA user_version=2;
CREATE TABLE signals(mailbox TEXT NOT NULL, seq INTEGER NOT NULL, id TEXT UNIQUE NOT NULL,
 PRIMARY KEY(mailbox,seq)) STRICT;
CREATE TABLE signal_sessions(signal TEXT PRIMARY KEY, session TEXT UNIQUE NOT NULL) STRICT;
CREATE TABLE session_tasks(task TEXT PRIMARY KEY, session TEXT NOT NULL, turn INTEGER NOT NULL, attempt INTEGER NOT NULL,
 UNIQUE(session,turn,attempt), FOREIGN KEY(session,turn) REFERENCES session_turns(session,turn)) STRICT;
