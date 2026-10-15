CREATE INDEX oauth_link_requests_session ON oauth_link_requests(session_id);
CREATE INDEX oauth_states_link_session ON oauth_states(link_session_id) WHERE link_session_id IS NOT NULL;
