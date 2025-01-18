DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_tables WHERE tablename = 'bulk_jobs') THEN
        CREATE TABLE bulk_jobs (
            id SERIAL PRIMARY KEY,
            created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
            total_records INTEGER NOT NULL
        );
    END IF;

    IF NOT EXISTS (SELECT 1 FROM pg_tables WHERE tablename = 'email_results') THEN
        CREATE TABLE email_results (
            id SERIAL PRIMARY KEY,
            job_id INTEGER NOT NULL REFERENCES bulk_jobs(id),
            result JSONB NOT NULL,
            created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP
        );
    END IF;
END $$;
