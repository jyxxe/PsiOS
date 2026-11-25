CREATE INDEX application_release_history ON versions((body->>'application_id'),seq)
    WHERE kind='ApplicationRelease' AND rev=1;
