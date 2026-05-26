-- Defense-in-depth: constrain task_queue status to valid lifecycle values.
ALTER TABLE task_queue
    ADD CONSTRAINT task_queue_status_check
    CHECK (status IN ('awaiting_approval', 'approved', 'rejected', 'running', 'done', 'failed'));
