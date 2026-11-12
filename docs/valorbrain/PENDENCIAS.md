# Pendências do fork

Defeitos que já existiam antes dos PRs #35 e #36 e que os revisores desses PRs encontraram em 11 e 12/11/2026. Ficam registrados aqui porque o fork não tem issues. Cada item também tem um ticket de feedback no ValorBrain. Quando um item for corrigido, o PR do conserto apaga a seção dele.

## 1. Deadlock entre `compact()` manual e o merge do trabalhador

Achado na revisão do #36.

**Reprodução** (sonda `stress.sh` do revisor, PG 18.6 com `shared_preload_libraries = pg_ripple`):
- configuração: `pg_ripple.merge_threshold = 10`, `pg_ripple.merge_interval_secs = 1`, `pg_ripple.worker_database` apontando para o banco do teste e `deadlock_timeout = 1s`;
- dados: 3 predicados com 300 triplas cada, promovidos (`SET pg_ripple.vp_promotion_threshold = 100`, `SELECT pg_ripple.promote_rare_predicates()`);
- carga: por 45 s, ao mesmo tempo, `SELECT pg_ripple.compact()` a cada 1,3 s e um escritor com `pg_ripple.insert_triple(...)` a cada 20 ms.

**Resultado:** de 1 a 3 deadlocks por rodada de 45 s:
- no 46ca4b83, 1 na `compact()`;
- no 725d62c2, 2 na `compact()` e 1 no trabalhador;
- numa rodada de 30 s, o trabalhador ainda teve 1 lock timeout.

```
ERROR:  deadlock detected
DETAIL:  Process 1578464 waits for AccessShareLock on relation 18221 of database 5; blocked by process 1576725.
        Process 1576725 waits for AccessShareLock on relation 18311 of database 5; blocked by process 1578464.
LOG:  pg_ripple merge worker: merge cycle panicked (1): [16908292] deadlock detected
```

No fim não sobra divergência: os padrões conferem com as tabelas VP, e o trabalhador volta no ciclo seguinte. Quem sai perdendo é a `compact()` do usuário, que termina em ERROR.

**Hipótese (não confirmada):** duas fusões rodam ao mesmo tempo, a da `compact()` e a do trabalhador. Cada uma segura uma trava forte nas tabelas de um predicado e espera AccessShare nas de outro: na troca do main, ou na leitura de todas as VP feita pelo rebuild dos padrões. O conserto provável é serializar a fusão da `compact()` e o ciclo do trabalhador com a mesma trava consultiva, ou travar as VP sempre na mesma ordem.

## 2. Segfault intermitente em `sparql_federation` (`register_endpoint`)

Achado na revisão do #35.

Numa rodada da suíte completa de pg_regress na main em que o #35 se baseava (418ba29d), o servidor caiu com segfault no teste `sparql_federation`, nesta chamada:

```sql
SELECT pg_ripple.register_endpoint('http://sparql.local/view-endpoint', 'test_stream');
```

O ambiente era o PG 18.6 do pgrx, com build de debug e sem preload. Os 41 testes que vieram depois da queda foram repetidos e deram saída idêntica. O segfault não voltou em 3 rodadas do PR nem em mais 1 da main: é intermitente. Não há core dump.

**Para reproduzir:** repetir `sparql_federation`, junto com os testes que vêm antes dele na ordem da suíte, em laço e com `ulimit -c unlimited` no postmaster, até capturar o core. Outra opção é rodar sob valgrind ou ASan. Por enquanto, a chamada acima é o único ponto conhecido.

## 3. `maybe_vacuum_dictionary` roda VACUUM por SPI

Achado na revisão do #36.

`maybe_vacuum_dictionary` (`src/dictionary/mod.rs:522`) roda `VACUUM ANALYZE _pg_ripple.dictionary` por `Spi::run` quando uma carga cria `pg_ripple.dict_vacuum_threshold` termos novos ou mais. Só que VACUUM não roda dentro de transação nem de função, então o PostgreSQL levanta ERROR. O `if let Err(e)` em volta só pega erro do SPI; o ERROR do PostgreSQL aborta a carga inteira.

**Reprodução:** o revisor reproduziu com `pg_ripple.dict_vacuum_threshold = 10000`, que é o valor da produção. Basta uma chamada que crie 10 mil termos novos, por exemplo:

```sql
SELECT pg_ripple.load_ntriples(string_agg(
  format('<https://v.test/s%s> <https://v.test/p> <https://v.test/o%s> .', i, i), E'\n'))
FROM generate_series(1, 5000) AS i;
```

A carga termina em ERROR e nada é gravado.

**Contorno:** `pg_ripple.dict_vacuum_threshold = 0`.

**Conserto provável:** trocar o VACUUM por `ANALYZE`, que roda dentro da transação, e deixar a limpeza para o autovacuum ou para um trabalhador que rode fora de transação.

## 4. Instalação nova: o apply worker cai com FATAL por falta de `_pg_ripple.cdc_lsn_watermark`

Achado na revisão do #36.

A tabela `_pg_ripple.cdc_lsn_watermark` só é criada em dois lugares:
- no script de atualização `sql/pg_ripple--0.80.0--0.81.0.sql`;
- em `cdc::initialize_cdc_schema()`, que só é chamada por `storage::initialize_schema()`, e essa função não é chamada por ninguém hoje (`#[allow(dead_code)]`).

O SQL de instalação da 0.141.0 não cria a tabela. Um banco que passou pela 0.81, como o da produção, tem a tabela; um `CREATE EXTENSION` novo não tem.

Com `pg_ripple.replication_enabled = on`, o apply worker lógico (`src/replication.rs:208-216`) grava o watermark a cada lote, por `cdc::update_lsn_watermark`. Sem a tabela, o `INSERT` falha com "relation does not exist", e o ERROR dentro de `BackgroundWorker::transaction` derruba o trabalhador com FATAL.

**Reprodução:**
1. Cluster novo com `shared_preload_libraries = pg_ripple`, `pg_ripple.replication_enabled = on` e `pg_ripple.worker_database` apontando para o banco.
2. `CREATE EXTENSION pg_ripple`.
3. Inserir linhas em `_pg_ripple.replication_status`.
4. O log mostra o FATAL do apply worker.

A sonda do revisor contorna o problema criando a tabela à mão:

```sql
CREATE TABLE IF NOT EXISTS _pg_ripple.cdc_lsn_watermark (
  slot_name TEXT PRIMARY KEY, last_lsn PG_LSN NOT NULL, updated_at TIMESTAMPTZ NOT NULL DEFAULT now());
```

**Conserto provável:** criar a tabela no SQL de instalação (`extension_sql!`) e também numa migração idempotente, para quem instalou direto numa versão que não tem a tabela.
