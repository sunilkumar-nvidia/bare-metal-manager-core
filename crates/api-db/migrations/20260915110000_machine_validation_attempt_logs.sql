CREATE TABLE machine_validation_attempt_logs (
    attempt_id UUID NOT NULL REFERENCES machine_validation_attempts(id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL,
    stream TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    content TEXT NOT NULL,
    PRIMARY KEY (attempt_id, sequence),
    CONSTRAINT machine_validation_attempt_logs_sequence_check CHECK (sequence > 0),
    CONSTRAINT machine_validation_attempt_logs_stream_check CHECK (stream IN ('stdout', 'stderr')),
    CONSTRAINT machine_validation_attempt_logs_chunk_size_check CHECK (octet_length(content) <= 16384)
);

CREATE INDEX machine_validation_attempt_logs_created_at_idx
    ON machine_validation_attempt_logs (created_at);
