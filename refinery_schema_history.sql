--
-- PostgreSQL database dump
--

-- Dumped from database version 14.17 (Homebrew)
-- Dumped by pg_dump version 14.17 (Homebrew)

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: refinery_schema_history; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.refinery_schema_history (
    version integer NOT NULL,
    name character varying(255),
    applied_on character varying(255),
    checksum character varying(255)
);


ALTER TABLE public.refinery_schema_history OWNER TO postgres;

--
-- Data for Name: refinery_schema_history; Type: TABLE DATA; Schema: public; Owner: postgres
--

COPY public.refinery_schema_history (version, name, applied_on, checksum) FROM stdin;
1	initial_schema	2025-11-01T03:39:13.717591Z	10646732297546135982
2	add_model_pricing	2025-11-01T03:39:13.960724Z	11960717503426879976
3	add_organization_limits_history	2025-11-01T03:39:13.970824Z	5747316317862795191
4	add_organization_usage_tracking	2025-11-01T03:39:13.976626Z	14169380272770353129
5	add_api_key_spend_limits	2025-11-01T03:39:13.988758Z	12297428768191682612
6	add_api_key_prefix	2025-11-01T03:39:13.991019Z	13074442770680175598
7	add_organization_invitations	2025-11-01T03:39:13.993521Z	8985576824080787278
8	add_model_backend_mappings	2025-11-01T03:39:14.000611Z	14504470414163630656
9	add_api_key_soft_delete	2025-11-01T03:39:14.006209Z	8095138122281394093
10	add_model_public_name	2025-11-01T03:39:14.00899Z	8460187396295389055
11	update_api_key_format_to_hyphen	2025-11-01T03:39:14.014089Z	13715479005066404357
12	fix_public_name_unique_constraint	2025-11-01T03:39:14.0166Z	14430581183126721024
13	refactor_organization_usage_log_model_references	2025-11-01T03:39:14.020486Z	11127613097952489424
14	remove_model_public_name	2025-11-01T03:39:14.034304Z	5514835069750113639
15	add_admin_access_token	2025-11-01T03:39:14.037482Z	14489656436502134405
16	add_tokens_revoked_at	2025-11-01T03:39:14.04654Z	13767586593316088827
17	rename_sessions_to_refresh_tokens	2025-11-01T03:39:14.048776Z	3853546558970906917
18	add_changed_by_user_id_to_limits_history	2025-11-01T03:39:14.050914Z	980806249034512334
19	add_changed_by_user_email_to_limits_history	2025-11-01T03:39:14.054014Z	4559690872933258769
\.


--
-- Name: refinery_schema_history refinery_schema_history_pkey; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.refinery_schema_history
    ADD CONSTRAINT refinery_schema_history_pkey PRIMARY KEY (version);


--
-- PostgreSQL database dump complete
--

