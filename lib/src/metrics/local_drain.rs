use std::str;
use std::time::Instant;
use time::OffsetDateTime;
use std::convert::TryInto;
use std::collections::BTreeMap;
use hdrhistogram::Histogram;
use sozu_command::proxy::{FilteredData,MetricsData,Percentiles,AppMetricsData,QueryMetricsType,QueryAnswerMetrics};

use super::{MetricData,Subscriber};

#[derive(Debug,Clone)]
pub enum AggregatedMetric {
  Gauge(usize),
  Count(i64),
  Time(Histogram<u32>)
}

impl AggregatedMetric {
  fn new(metric: MetricData) -> AggregatedMetric {
    match metric {
      MetricData::Gauge(value) => AggregatedMetric::Gauge(value),
      MetricData::GaugeAdd(value) => AggregatedMetric::Gauge(value as usize),
      MetricData::Count(value) => AggregatedMetric::Count(value),
      MetricData::Time(value)  => {
        //FIXME: do not unwrap here
        let mut h = ::hdrhistogram::Histogram::new(3).unwrap();
        if let Err(e) = h.record(value as u64) {
          error!("could not create histogram with time metric {:?}: {:?}", value, e);
        }
        AggregatedMetric::Time(h)
      }
    }
  }

  fn update(&mut self, key: &'static str, m: MetricData) {
    match (self, m) {
      (&mut AggregatedMetric::Gauge(ref mut v1), MetricData::Gauge(v2)) => {
        *v1 = v2;
      },
      (&mut AggregatedMetric::Gauge(ref mut v1), MetricData::GaugeAdd(v2)) => {
        *v1 = (*v1 as i64 + v2) as usize;
      },
      (&mut AggregatedMetric::Count(ref mut v1), MetricData::Count(v2)) => {
        *v1 += v2;
      },
      (&mut AggregatedMetric::Time(ref mut v1), MetricData::Time(v2)) => {
        if let Err(e) = (*v1).record(v2 as u64) {
          error!("could not add time metric {}={:?} to histogram: {:?}", key, v2, e);
        }
      },
      (s,m) => panic!("tried to update metric {} of value {:?} with an incompatible metric: {:?}", key, s, m)
    }
  }
}

pub fn histogram_to_percentiles(hist: &Histogram<u32>) -> Percentiles {
  Percentiles {
    samples:  hist.len(),
    p_50:     hist.value_at_percentile(50.0),
    p_90:     hist.value_at_percentile(90.0),
    p_99:     hist.value_at_percentile(99.0),
    p_99_9:   hist.value_at_percentile(99.9),
    p_99_99:  hist.value_at_percentile(99.99),
    p_99_999: hist.value_at_percentile(99.999),
    p_100:    hist.value_at_percentile(100.0),
  }
}

pub fn aggregated_to_filtered(value: &AggregatedMetric) -> FilteredData {
  match value {
    &AggregatedMetric::Gauge(i) => FilteredData::Gauge(i),
    &AggregatedMetric::Count(i) => FilteredData::Count(i),
    &AggregatedMetric::Time(ref hist) => {
      FilteredData::Percentiles(histogram_to_percentiles(&hist))
    },
  }
}

#[derive(Clone,Debug)]
pub struct AppMetrics {
  pub data: BTreeMap<String, AggregatedMetric>,
  pub backend_data: BTreeMap<String, BTreeMap<String, AggregatedMetric>>,
}

#[derive(Clone,Debug)]
pub struct BackendMetrics {
  pub cluster_id: String,
  pub data:   BTreeMap<String, AggregatedMetric>,
}

#[derive(Clone,Debug,PartialEq)]
enum MetricKind {
  Gauge,
  Count,
  Time,
}

#[derive(Clone,Debug,PartialEq)]
enum MetricMeta {
    Cluster,
    ClusterBackend,
}

#[derive(Debug)]
pub struct LocalDrain {
  pub prefix:          String,
  pub created:         Instant,
  pub db:              sled::Db,
  pub cluster_tree:    sled::Tree,
  pub backend_tree:    sled::Tree,
  pub data:            BTreeMap<String, AggregatedMetric>,
  metrics:             BTreeMap<String, (MetricMeta, MetricKind)>,
  use_tagged_metrics:  bool,
  origin:              String,
}

impl LocalDrain {
  pub fn new(prefix: String) -> Self {
    let db = sled::Config::new()
        .temporary(true)
        .mode(sled::Mode::LowSpace)
        .open()
        .unwrap();
    let cluster_tree = db.open_tree("cluster").unwrap();
    let backend_tree = db.open_tree("backend").unwrap();

    LocalDrain {
      prefix,
      created:     Instant::now(),
      db,
      cluster_tree,
      backend_tree,
      metrics:     BTreeMap::new(),
      data:        BTreeMap::new(),
      use_tagged_metrics: false,
      origin:      String::from("x"),
    }
  }

  pub fn dump_metrics_data(&mut self) -> MetricsData {
    MetricsData {
      proxy:    self.dump_process_data(),
      clusters: self.dump_cluster_data().map_err(|e| {
          error!("metrics database error: {:?}", e);
      }).unwrap_or_else(|_| BTreeMap::new()),
    }
  }

  pub fn dump_process_data(&mut self) -> BTreeMap<String, FilteredData> {
    let data: BTreeMap<String, FilteredData> = self.data.iter().map(|(ref key, ref value)| {
      (key.to_string(), aggregated_to_filtered(value))
    }).collect();

    data
  }

  pub fn query(&mut self, q: &QueryMetricsType) -> Result<QueryAnswerMetrics, String> {
      info!("GOT QUERY: {:?}", q);
      match q {
          QueryMetricsType::List => {
              Ok(QueryAnswerMetrics::List(self.metrics.keys().cloned().collect()))
          },
          QueryMetricsType::Cluster { metrics, clusters } => {
              self.query_cluster(metrics, clusters).map_err(|e| {
                  error!("metrics database error: {:?}", e);
                  format!("metrics database error: {:?}", e)
              })
          },
          QueryMetricsType::Backend { metrics, backends } => {
              self.query_backend(metrics, backends).map_err(|e| {
                  error!("metrics database error: {:?}", e);
                  format!("metrics database error: {:?}", e)
              })
          },
      }
  }

  fn query_cluster(&mut self, metrics: &Vec<String>, clusters: &Vec<String>) -> Result<QueryAnswerMetrics, sled::Error> {
      let mut apps: BTreeMap<String, BTreeMap<String, FilteredData>> = BTreeMap::new();
      for cluster_id in clusters.iter() {
          apps.insert(cluster_id.to_string(), BTreeMap::new());
      }

      trace!("current metrics: {:#?}", self.metrics);
      for prefix_key in metrics.iter() {
          for cluster_id in clusters.iter() {
              let key = format!("{}\t{}", prefix_key, cluster_id);

              let res = self.metrics.get(&key);
              if res.is_none() {
                  error!("unknown metric key {}", key);
                  continue
              }
              let (meta, kind) = res.unwrap();

              //FIXME: check here that the metric is a cluster level one

              if *kind == MetricKind::Time {
                  let mut percentiles = Percentiles::default();

                  let count_key = format!("{}\t{}.count ", prefix_key, cluster_id);
                  let count_end = format!("{}\x7F", count_key);

                  if let Some(v) = self.get_last_before(&count_key, &count_end, false)? {
                      let value = usize::from_le_bytes((*v).try_into().unwrap());
                      //info!("count -> {} ({:?})", value, *v);
                      percentiles.samples = value as u64;
                  }

                  {
                      let p50_key = format!("{}\t{}.p50 ", prefix_key, cluster_id);
                      let p50_end = format!("{}\x7F", p50_key);

                      if let Some(v) = self.get_last_before(&p50_key, &p50_end, false)? {
                          let value = usize::from_le_bytes((*v).try_into().unwrap());
                          //info!("p50 -> {} ({:?})", value, *v);
                          percentiles.p_50 = value as u64;
                      }
                  }

                  {
                      let p90_key = format!("{}\t{}.p90 ", prefix_key, cluster_id);
                      let p90_end = format!("{}\x7F", p90_key);

                      if let Some(v) = self.get_last_before(&p90_key, &p90_end, false)? {
                          let value = usize::from_le_bytes((*v).try_into().unwrap());
                          //info!("p90 -> {} ({:?})", value, *v);
                          percentiles.p_90 = value as u64;
                      }
                  }

                  {
                      let p99_key = format!("{}\t{}.p99 ", prefix_key, cluster_id);
                      let p99_end = format!("{}\x7F", p99_key);

                      if let Some(v) = self.get_last_before(&p99_key, &p99_end, false)? {
                          let value = usize::from_le_bytes((*v).try_into().unwrap());
                          //info!("p99 -> {} ({:?})", value, *v);
                          percentiles.p_99 = value as u64;
                      }
                  }

                  {
                      let p99_9_key = format!("{}\t{}.p99.9 ", prefix_key, cluster_id);
                      let p99_9_end = format!("{}\x7F", p99_9_key);

                      if let Some(v) = self.get_last_before(&p99_9_key, &p99_9_end, false)? {
                          let value = usize::from_le_bytes((*v).try_into().unwrap());
                          //info!("p99.9 -> {} ({:?})", value, *v);
                          percentiles.p_99_9 = value as u64;
                      }
                  }

                  {
                      let p99_99_key = format!("{}\t{}.p99.99 ", prefix_key, cluster_id);
                      let p99_99_end = format!("{}\x7F", p99_99_key);

                      if let Some(v) = self.get_last_before(&p99_99_key, &p99_99_end, false)? {
                          let value = usize::from_le_bytes((*v).try_into().unwrap());
                          //info!("p99.99 -> {} ({:?})", value, *v);
                          percentiles.p_99_99 = value as u64;
                      }
                  }

                  {
                      let p99_999_key = format!("{}\t{}.p99.999 ", prefix_key, cluster_id);
                      let p99_999_end = format!("{}\x7F", p99_999_key);

                      if let Some(v) = self.get_last_before(&p99_999_key, &p99_999_end, false)? {
                          let value = usize::from_le_bytes((*v).try_into().unwrap());
                          //info!("p99.999 -> {} ({:?})", value, *v);
                          percentiles.p_99_999 = value as u64;
                      }
                  }

                  {
                      let p100_key = format!("{}\t{}.p100 ", prefix_key, cluster_id);
                      let p100_end = format!("{}\x7F", p100_key);

                      if let Some(v) = self.get_last_before(&p100_key, &p100_end, false)? {
                          let value = usize::from_le_bytes((*v).try_into().unwrap());
                          //info!("p100 -> {} ({:?})", value, *v);
                          percentiles.p_100 = value as u64;
                      }
                  }

                  apps.get_mut(cluster_id).unwrap()
                      .insert(key.to_string(), FilteredData::Percentiles(percentiles));

                  continue;
              }

              let end = format!("{}\x7F", key);

              if let Some(v) = self.get_last_before(&key, &end, false)? {
                  match kind {
                      MetricKind::Gauge => {
                          apps.get_mut(cluster_id).unwrap().insert(key.to_string(), FilteredData::Gauge(usize::from_le_bytes((*v).try_into().unwrap())));
                      },
                      MetricKind::Count => {
                          apps.get_mut(cluster_id).unwrap().insert(key.to_string(), FilteredData::Count(i64::from_le_bytes((*v).try_into().unwrap())));
                      },
                      MetricKind::Time => {}
                  }
              }
          }
      }

      trace!("query result: {:#?}", apps);
      Ok(QueryAnswerMetrics::Cluster(apps))
  }

  fn query_backend(&mut self, metrics: &Vec<String>, backends: &Vec<(String,String)>) -> Result<QueryAnswerMetrics, sled::Error> {
      let mut backend_data: BTreeMap<String, BTreeMap<String, BTreeMap<String, FilteredData>>> = BTreeMap::new();
      for (cluster_id, backend_id) in backends.iter() {
          let t = backend_data.entry(cluster_id.to_string()).or_insert_with(BTreeMap::new);
          t.insert(backend_id.to_string(), BTreeMap::new());
      }

      trace!("current metrics: {:#?}", self.metrics);
      for prefix_key in metrics.iter() {
          for (cluster_id, backend_id) in backends.iter() {
              let key = format!("{}\t{}\t{}", prefix_key, cluster_id, backend_id);

              let res = self.metrics.get(&key);
              if res.is_none() {
                  error!("unknown metric key {}", key);
                  continue
              }
              let (meta, kind) = res.unwrap();

              let end = format!("{}\x7F", key);
              if let Some(v) = self.get_last_before(&key, &end, true)? {
                  match kind {
                      MetricKind::Gauge => {
                          backend_data.get_mut(cluster_id).unwrap()
                              .get_mut(backend_id).unwrap().insert(key.to_string(), FilteredData::Gauge(usize::from_le_bytes((*v).try_into().unwrap())));
                      },
                      MetricKind::Count => {
                          backend_data.get_mut(cluster_id).unwrap()
                              .get_mut(backend_id).unwrap().insert(key.to_string(), FilteredData::Count(i64::from_le_bytes((*v).try_into().unwrap())));
                      },
                      MetricKind::Time => {
                          //unimplemented for now
                      }
                  }
              }
          }
      }

      trace!("query result: {:#?}", backend_data);
      Ok(QueryAnswerMetrics::Backend(backend_data))
  }

  fn get_last_before(&self, start: &str, end: &str, is_backend: bool) -> Result<Option<sled::IVec>, sled::Error> {
      let tree = if is_backend {
          &self.backend_tree
      } else {
          &self.cluster_tree
      };

      if let Some((k, v)) = tree.get_lt(end.as_bytes())? {
          if k.starts_with(start.as_bytes()) {

              if is_backend {
                  let mut it = k.split(|c: &u8| *c == b'\t');
                  let key = std::str::from_utf8(it.next().unwrap()).unwrap();
                  let cluster_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                  let backend_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                  let timestamp:&str = std::str::from_utf8(it.next().unwrap()).unwrap();//.parse().unwrap();

                  let value = usize::from_le_bytes((*v).try_into().unwrap());
                  info!("looking at key = {}, id = {}, backend_id = {}, ts = {} -> {}",
                        key, cluster_id, backend_id, timestamp, value);
              } else {
                  info!("current key: {}", std::str::from_utf8(&k).unwrap());
                  let mut it = k.split(|c: &u8| *c == b'\t');
                  let key = std::str::from_utf8(it.next().unwrap()).unwrap();
                  let cluster_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                  let timestamp:&str = std::str::from_utf8(it.next().unwrap()).unwrap();//.parse().unwrap();

                  let value = usize::from_le_bytes((*v).try_into().unwrap());
                  info!("looking at key = {}, id = {}, ts = {} -> {}",
                        key, cluster_id, timestamp, value);
              }

              return Ok(Some(v));
          } else {
              error!("no key found between '{}' and '{}'", start, end);
          }
      }

      Ok(None)
  }

  pub fn dump_cluster_data(&mut self) -> Result<BTreeMap<String,AppMetricsData>, sled::Error> {
      let mut apps = BTreeMap::new();

      for (key, (meta, kind)) in self.metrics.iter() {
          let end = format!("{}\x7F", key);

          match meta {
              MetricMeta::Cluster => {
                  for res in self.cluster_tree.range(key.as_bytes()..end.as_bytes()) {
                      let (k, v) = res?;

                      let mut it = k.split(|c: &u8| *c == b'\t');
                      let key = std::str::from_utf8(it.next().unwrap()).unwrap();
                      let app_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                      //let timestamp:i64 = std::str::from_utf8(it.next().unwrap()).unwrap().parse().unwrap();
                      let timestamp = std::str::from_utf8(it.next().unwrap()).unwrap();//.parse().unwrap();

                      info!("looking at key = {}, id = {}, ts = {}",
                            key, app_id, timestamp);

                      let metrics_data = apps.entry(app_id.to_string()).or_insert_with(AppMetricsData::new);
                      match kind {
                          MetricKind::Gauge => {
                              /*if metrics_data.data.contains_key(key) {
                                  let v2 = metrics_data.data.get(key).unwrap().clone();
                              } else {*/
                                  metrics_data.data.insert(key.to_string(), FilteredData::Gauge(usize::from_le_bytes((*v).try_into().unwrap())));
                              //}
                          },
                          MetricKind::Count => {
                              /*if metrics_data.data.contains_key(key) {
                                  let v2 = metrics_data.data.get(key).unwrap().clone();
                              } else {*/
                                  metrics_data.data.insert(key.to_string(), FilteredData::Count(i64::from_le_bytes((*v).try_into().unwrap())));
                              //}
                          },
                          MetricKind::Time => {
                              //unimplemented for now
                          }
                      }
                  }
              },
              MetricMeta::ClusterBackend => {
                  for res in self.backend_tree.range(key.as_bytes()..end.as_bytes()) {
                      let (k, v) = res?;

                      let mut it = k.split(|c: &u8| *c == b'\t');
                      let key = std::str::from_utf8(it.next().unwrap()).unwrap();
                      let app_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                      let backend_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                      //let timestamp:i64 = std::str::from_utf8(it.next().unwrap()).unwrap().parse().unwrap();
                      let timestamp = std::str::from_utf8(it.next().unwrap()).unwrap();//.parse().unwrap();

                      info!("looking at key = {}, cluster id = {}, bid: {}, ts = {}",
                            key, app_id, backend_id, timestamp);

                      let app_metrics_data = apps.entry(app_id.to_string()).or_insert_with(AppMetricsData::new);
                      let backend_metrics_data = app_metrics_data.backends.entry(backend_id.to_string()).or_insert_with(BTreeMap::new);
                      match kind {
                          MetricKind::Gauge => {
                              /*if backend_metrics_data.contains_key(key) {
                                  let v2 = backend_metrics_data.get(key).unwrap().clone();
                              } else {*/
                                  backend_metrics_data.insert(key.to_string(), FilteredData::Gauge(usize::from_le_bytes((*v).try_into().unwrap())));
                              //}
                          },
                          MetricKind::Count => {
                              /*if backend_metrics_data.contains_key(key) {
                                  let v2 = backend_metrics_data.get(key).unwrap().clone();
                              } else {*/
                                  backend_metrics_data.insert(key.to_string(), FilteredData::Count(i64::from_le_bytes((*v).try_into().unwrap())));
                              //}
                          },
                          MetricKind::Time => {
                              //unimplemented for now
                          }
                      }
                  }
              },
          }
      }


      // still clear the DB for now
      //self.db.clear();

      Ok(apps)
  }

  fn receive_cluster_metric(&mut self, key: &str, cluster_id: &str, backend_id: Option<&str>, metric: MetricData) {
      info!("metric: {} {} {:?} {:?}", key, cluster_id, backend_id, metric);

      if let MetricData::Time(t) = metric {
         if let Err(e) = self.store_time_metric(key, cluster_id, None, t) {
             error!("metrics database error: {:?}", e);
         }
         if let Some(bid) = backend_id {
             if let Err(e) = self.store_time_metric(key, cluster_id, backend_id, t) {
                 error!("metrics database error: {:?}", e);
             }
         }
      } else {
          if let Err(e) = self.store_metric(&format!("{}\t{}", key, cluster_id), cluster_id, None, &metric) {
              error!("metrics database error: {:?}", e);
          }
          if let Some(bid) = backend_id {
              if let Err(e) = self.store_metric(&format!("{}\t{}\t{}", key, cluster_id, bid), cluster_id, backend_id, &metric) {
                  error!("metrics database error: {:?}", e);
              }
          }
      }
  }

  fn store_metric(&mut self, key_prefix: &str, id: &str, backend_id: Option<&str>, metric: &MetricData) -> Result<(), sled::Error> {
      info!("metric: {} {} {:?} {:?}", key_prefix, id, backend_id, metric);

      if !self.metrics.contains_key(key_prefix) {
          let kind = match metric {
              MetricData::Gauge(_) => MetricKind::Gauge,
              MetricData::GaugeAdd(_) => MetricKind::Gauge,
              MetricData::Count(_) => MetricKind::Count,
              MetricData::Time(_) => MetricKind::Time,
          };
          let meta = if backend_id.is_some() {
              MetricMeta::ClusterBackend
          } else {
              MetricMeta::Cluster
          };

          self.metrics.insert(key_prefix.to_string(), (meta, kind));
          let end = format!("{}\x7F", key_prefix);
          if backend_id.is_some() {
              self.backend_tree.insert(end.as_bytes(), &0u64.to_le_bytes())?;
          } else {
              self.cluster_tree.insert(end.as_bytes(), &0u64.to_le_bytes())?;
          }
      }

      match metric {
          MetricData::Gauge(i) => {
              self.store_gauge(&key_prefix, *i, backend_id.is_some())?;
          },
          MetricData::GaugeAdd(i) => {
              self.add_gauge(&key_prefix, *i, backend_id.is_some())?;
          },
          MetricData::Count(i) => {
              self.store_count(&key_prefix, *i, backend_id.is_some())?;
          },
          MetricData::Time(i) => {
              //self.store_time(&key_prefix, *i, backend_id.is_some())?;
          },
      }

      /*
      if let (Some(first), Some(second)) = (self.db.first().unwrap(), self.db.last().unwrap()) {
        for res in self.db.range(first.0..second.0) {
            let (k, v) = res.unwrap();
            info!("{} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));

        }
      }
      //info!("metrics: {:?}", self.metrics);
      info!("db size: {:?}", self.db.size_on_disk());
      */

      Ok(())
  }

  fn store_gauge(&mut self, key: &str, i: usize, is_backend: bool) -> Result<(), sled::Error> {
      let now = OffsetDateTime::now_utc();
      let timestamp = now.unix_timestamp();
      let complete_key = format!("{}\t{}", key, timestamp);

      info!("store gauge at {} -> {}", complete_key, i);
      if is_backend {
          self.backend_tree.insert(complete_key.as_bytes(), &i.to_le_bytes())?;
      } else {
          self.cluster_tree.insert(complete_key.as_bytes(), &i.to_le_bytes())?;
      }

      // we change the minute, aggregate the 60 measurements from the last minute
      if now.second() == 0 {
          self.aggregate_gauge(key, now, is_backend)?;
      }

      Ok(())
  }

  fn add_gauge(&mut self, key: &str, i: i64, is_backend: bool) -> Result<(), sled::Error> {
      let now = OffsetDateTime::now_utc();
      let timestamp = now.unix_timestamp();
      let complete_key = format!("{}\t{}", key, timestamp);
      let one_minute_ago = format!("{}\t{}", key, timestamp - 60);

      let tree = if is_backend {
          &mut self.backend_tree
      } else {
          &mut self.cluster_tree
      };
      info!("add gauge at {} -> {}", complete_key, i);
      match tree.range(one_minute_ago.as_bytes()..=complete_key.as_bytes()).rev().next() {
          None => {
              tree.insert(complete_key.as_bytes(), &i.to_le_bytes())?;
          },
          Some(Err(e)) => return Err(e),
          Some(Ok((_, v))) => {
              let i2 = i64::from_le_bytes((*v).try_into().unwrap());
              tree.insert(complete_key.as_bytes(), &(i+i2).to_le_bytes())?;
          }
      };

      // we change the minute, aggregate the 60 measurements from the last minute
      if now.second() == 0 {
          self.aggregate_gauge(key, now, is_backend)?;
      }

      Ok(())
  }

  fn store_count(&mut self, key: &str, i: i64, is_backend: bool) -> Result<(), sled::Error> {
      let now = OffsetDateTime::now_utc();
      let timestamp = now.unix_timestamp();
      let complete_key = format!("{}\t{}", key, timestamp);

      let tree = if is_backend {
          &mut self.backend_tree
      } else {
          &mut self.cluster_tree
      };
      info!("store count at {} -> {}", complete_key, i);
      match tree.get(complete_key.as_bytes())? {
          None => {
              tree.insert(complete_key.as_bytes(), &i.to_le_bytes())?;
          },
          Some(v) => {
              let i2 = i64::from_le_bytes((*v).try_into().unwrap());
              tree.insert(complete_key.as_bytes(), &(i+i2).to_le_bytes())?;
          }
      };

      // we change the minute, aggregate the 60 measurements from the last minute
      if now.second() == 0 {
          self.aggregate_count(key, now, is_backend)?;
      }

      Ok(())
  }

  fn aggregate_gauge(&mut self, key: &str, now: OffsetDateTime, is_backend: bool) -> Result<(), sled::Error> {
      let timestamp = now.unix_timestamp();
      let one_hour_ago = format!("{}\t{}", key, timestamp - 3600);
      let one_minute_ago = format!("{}\t{}", key, timestamp - 60);
      let now_key = format!("{}\t{}", key, timestamp);

      let tree = if is_backend {
          &mut self.backend_tree
      } else {
          &mut self.cluster_tree
      };

      // aggregate 60 measures in a point at the last minute
      let mut value = None;
      for res in tree.range(one_minute_ago.as_bytes()..now_key.as_bytes()) {
          let (k, v) = res?;
          value = Some(usize::from_le_bytes((*v).try_into().unwrap()));
          info!("removing {} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));
          tree.remove(k)?;
      }

      if let Some(v) = value {
          info!("reinserting {} -> {:?}", one_minute_ago, v);
          tree.insert(one_minute_ago.as_bytes(), &v.to_le_bytes())?;
      }

      // aggregate 60 measures in a point at the last hour
      if now.minute() == 0 {
          let mut value = None;
          for res in tree.range(one_hour_ago.as_bytes()..one_minute_ago.as_bytes()) {
              let (k, v) = res?;
              value = Some(usize::from_le_bytes((*v).try_into().unwrap()));
              info!("removing {} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));
              tree.remove(k)?;
          }

          if let Some(v) = value {
              info!("reinserting {} -> {:?}", one_hour_ago, v);
              tree.insert(one_minute_ago.as_bytes(), &v.to_le_bytes())?;
          }

          // remove all measures older than 24h
          let one_day_ago = format!("{}\t{}", key, timestamp - 3600 * 24);
          for res in tree.range(key.as_bytes()..one_day_ago.as_bytes()) {
              let (k, v) = res?;
              value = Some(usize::from_le_bytes((*v).try_into().unwrap()));
              info!("removing {} -> {:?} (more than 24h)", unsafe { std::str::from_utf8_unchecked(&k) }, value);
              tree.remove(k)?;
          }
      }

      Ok(())
  }

  fn aggregate_count(&mut self, key: &str, now: OffsetDateTime, is_backend: bool) -> Result<(), sled::Error> {
      let timestamp = now.unix_timestamp();
      let one_hour_ago = format!("{}\t{}", key, timestamp - 3600);
      let one_minute_ago = format!("{}\t{}", key, timestamp - 60);
      let now_key = format!("{}\t{}", key, timestamp);

      let tree = if is_backend {
          &mut self.backend_tree
      } else {
          &mut self.cluster_tree
      };

      // aggregate 60 measures in a point at the last hour
      let mut value = 0i64;
      let mut found = false;
      for res in tree.range(one_minute_ago.as_bytes()..now_key.as_bytes()) {
          found = true;
          let (k, v) = res?;
          value += i64::from_le_bytes((*v).try_into().unwrap());
          info!("removing {} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));
          tree.remove(k)?;
      }

      if found {
          info!("reinserting {} -> {:?}", one_minute_ago, value);
          tree.insert(one_minute_ago.as_bytes(), &value.to_le_bytes())?;
      }

      // remove all measures older than 24h
      if now.minute() == 0 {
          let mut value = 0i64;
          let mut found = false;
          for res in tree.range(one_hour_ago.as_bytes()..one_minute_ago.as_bytes()) {
              found = true;
              let (k, v) = res?;
              value += i64::from_le_bytes((*v).try_into().unwrap());
              info!("removing {} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));
              tree.remove(k)?;
          }

          if found {
              info!("reinserting {} -> {:?}", one_hour_ago, value);
              tree.insert(one_hour_ago.as_bytes(), &value.to_le_bytes())?;
          }

          // remove all measures older than 24h
          let one_day_ago = format!("{}\t{}", key, timestamp - 3600 * 24);
          for res in tree.range(key.as_bytes()..one_day_ago.as_bytes()) {
              let (k, v) = res?;
              value = i64::from_le_bytes((*v).try_into().unwrap());
              info!("removing {} -> {:?} (more than 24h)", unsafe { std::str::from_utf8_unchecked(&k) }, value);
              tree.remove(k)?;
          }
      }

      Ok(())
  }

  fn store_time_metric(&mut self, key: &str, cluster_id: &str, backend_id: Option<&str>, t: usize) -> Result<(), sled::Error> {
      let now = OffsetDateTime::now_utc();
      let timestamp = now.unix_timestamp();
      let _res = self.store_time_metric_at(key, cluster_id, backend_id, timestamp, t)?;

      let second = now.second();
      // we also aggregate at second zero
      if second != 0 {
          let previous_minute = now - time::Duration::seconds(second as i64);
          let timestamp = previous_minute.unix_timestamp();
          info!("WILL REWRITE TIME METRIC AT {}", timestamp);
          let _res = self.store_time_metric_at(key, cluster_id, backend_id, timestamp, t)?;
          //self.aggregate_count(key, now, is_backend)?;
      } else {
          //FIXME: here we should delete all the measurements for the previous 60 seconds
      }

      Ok(())
    }

  fn store_time_metric_at(&mut self, key: &str, cluster_id: &str,
                          backend_id: Option<&str>, timestamp: i64, t: usize) -> Result<(), sled::Error> {
      let key_prefix = if let Some(bid) = backend_id {
          format!("{}\t{}\t{}", key, cluster_id, bid)
      } else {
          format!("{}\t{}", key, cluster_id)
      };

      let count_key_prefix = format!("{}.count ", key_prefix);
      let mean_key_prefix = format!("{}.mean ", key_prefix);
      let var_key_prefix = format!("{}.var ", key_prefix);
      let p50_key_prefix = format!("{}.p50 ", key_prefix);
      let p90_key_prefix = format!("{}.p90 ", key_prefix);
      let p99_key_prefix = format!("{}.p99 ", key_prefix);
      let p99_9_key_prefix = format!("{}.p99.9 ", key_prefix);
      let p99_99_key_prefix = format!("{}.p99.99 ", key_prefix);
      let p99_999_key_prefix = format!("{}.p99.999 ", key_prefix);
      let p100_key_prefix = format!("{}.p100 ", key_prefix);

      if !self.metrics.contains_key(&key_prefix) {
          let meta = if backend_id.is_some() {
              MetricMeta::ClusterBackend
          } else {
              MetricMeta::Cluster
          };

          self.metrics.insert(key_prefix.to_string(), (meta, MetricKind::Time));

          let count_end = format!("{}\x7F", count_key_prefix);
          let mean_end = format!("{}\x7F", mean_key_prefix);
          let var_end = format!("{}\x7F", var_key_prefix);
          let p50_end = format!("{}\x7F", p50_key_prefix);
          let p90_end = format!("{}\x7F", p90_key_prefix);
          let p99_end = format!("{}\x7F", p99_key_prefix);
          let p99_9_end = format!("{}\x7F", p99_9_key_prefix);
          let p99_99_end = format!("{}\x7F", p99_99_key_prefix);
          let p99_999_end = format!("{}\x7F", p99_999_key_prefix);
          let p100_end = format!("{}\x7F", p100_key_prefix);
          if backend_id.is_some() {
              self.backend_tree.insert(count_end.as_bytes(), &0u64.to_le_bytes())?;
              self.backend_tree.insert(mean_end.as_bytes(), &0u64.to_le_bytes())?;
              self.backend_tree.insert(var_end.as_bytes(), &0u64.to_le_bytes())?;
              self.backend_tree.insert(p50_end.as_bytes(), &0u64.to_le_bytes())?;
              self.backend_tree.insert(p90_end.as_bytes(), &0u64.to_le_bytes())?;
              self.backend_tree.insert(p99_end.as_bytes(), &0u64.to_le_bytes())?;
              self.backend_tree.insert(p99_9_end.as_bytes(), &0u64.to_le_bytes())?;
              self.backend_tree.insert(p99_99_end.as_bytes(), &0u64.to_le_bytes())?;
              self.backend_tree.insert(p99_999_end.as_bytes(), &0u64.to_le_bytes())?;
              self.backend_tree.insert(p100_end.as_bytes(), &0u64.to_le_bytes())?;
          } else {
              self.cluster_tree.insert(count_end.as_bytes(), &0u64.to_le_bytes())?;
              self.cluster_tree.insert(mean_end.as_bytes(), &0u64.to_le_bytes())?;
              self.cluster_tree.insert(var_end.as_bytes(), &0u64.to_le_bytes())?;
              self.cluster_tree.insert(p50_end.as_bytes(), &0u64.to_le_bytes())?;
              self.cluster_tree.insert(p90_end.as_bytes(), &0u64.to_le_bytes())?;
              self.cluster_tree.insert(p99_end.as_bytes(), &0u64.to_le_bytes())?;
              self.cluster_tree.insert(p99_9_end.as_bytes(), &0u64.to_le_bytes())?;
              self.cluster_tree.insert(p99_99_end.as_bytes(), &0u64.to_le_bytes())?;
              self.cluster_tree.insert(p99_999_end.as_bytes(), &0u64.to_le_bytes())?;
              self.cluster_tree.insert(p100_end.as_bytes(), &0u64.to_le_bytes())?;
          }
      }

      let tree = if backend_id.is_some() {
          &mut self.backend_tree
      } else {
          &mut self.cluster_tree
      };

      let count_key = format!("{}\t{}", count_key_prefix, timestamp);
      let mean_key = format!("{}\t{}", mean_key_prefix, timestamp);
      let var_key = format!("{}\t{}", var_key_prefix, timestamp);
      let p50_key = format!("{}\t{}", p50_key_prefix, timestamp);
      let p90_key = format!("{}\t{}", p90_key_prefix, timestamp);
      let p99_key = format!("{}\t{}", p99_key_prefix, timestamp);
      let p99_9_key = format!("{}\t{}", p99_9_key_prefix, timestamp);
      let p99_99_key = format!("{}\t{}", p99_99_key_prefix, timestamp);
      let p99_999_key = format!("{}\t{}", p99_999_key_prefix, timestamp);
      let p100_key = format!("{}\t{}", p100_key_prefix, timestamp);

      match tree.get(count_key.as_bytes())? {
          None => {
              tree.insert(count_key.as_bytes(), &1usize.to_le_bytes())?;
              tree.insert(mean_key.as_bytes(), &(t as f64).to_le_bytes())?;
              tree.insert(var_key.as_bytes(), &0f64.to_le_bytes())?;
              tree.insert(p50_key.as_bytes(), &t.to_le_bytes())?;
              tree.insert(p90_key.as_bytes(), &t.to_le_bytes())?;
              tree.insert(p99_key.as_bytes(), &t.to_le_bytes())?;
              tree.insert(p99_9_key.as_bytes(), &t.to_le_bytes())?;
              tree.insert(p99_99_key.as_bytes(), &t.to_le_bytes())?;
              tree.insert(p99_999_key.as_bytes(), &t.to_le_bytes())?;
              tree.insert(p100_key.as_bytes(), &t.to_le_bytes())?;
              info!("TIME stored new {}: {}", p50_key, t);
          },
          Some(v) => {
              let old_count = i64::from_le_bytes((*v).try_into().unwrap());
              tree.insert(count_key.as_bytes(), &(old_count+1).to_le_bytes())?;

              match tree.get(mean_key.as_bytes())? {
                  None => {
                      tree.insert(mean_key.as_bytes(), &t.to_le_bytes())?;
                  },
                  Some(mean_v) => {
                      let old_mean = f64::from_le_bytes((*mean_v).try_into().unwrap());
                      let new_mean = (old_mean * old_count as f64 + t as f64) / (old_count as f64 + 1f64);

                      tree.insert(mean_key.as_bytes(), &new_mean.to_le_bytes())?;

                      match tree.get(var_key.as_bytes())? {
                          None => {
                              tree.insert(var_key.as_bytes(), &0f64.to_le_bytes())?;
                          },
                          Some(var_v) => {
                              let old_var = f64::from_le_bytes((*var_v).try_into().unwrap());
                              let deviation = t as f64 - old_mean;
                              let new_var = (old_var * old_count as f64 + deviation * deviation) / (old_count as f64 +1f64);
                              tree.insert(var_key.as_bytes(), &new_var.to_le_bytes())?;

                              let standard_dev = new_var.sqrt();

                              if let Some(old_v) = tree.get(p50_key.as_bytes())? {
                                  let old_percentile = usize::from_le_bytes((*old_v).try_into().unwrap());
                                  let new_percentile = calculate_percentile(old_percentile, t,
                                                                            standard_dev, 0.50f64);
                                  tree.insert(p50_key.as_bytes(), &new_percentile.to_le_bytes())?;
                                  info!("TIME rewrote {}: {} (old={}, t={})", p50_key, new_percentile,
                                    old_percentile, t);
                              }

                              if let Some(old_v) = tree.get(p90_key.as_bytes())? {
                                  let old_percentile = usize::from_le_bytes((*old_v).try_into().unwrap());
                                  let new_percentile = calculate_percentile(old_percentile, t,
                                                                            standard_dev, 0.90f64);
                                  tree.insert(p90_key.as_bytes(), &new_percentile.to_le_bytes())?;
                                  info!("TIME rewrote {}: {} (old={}, t={})", p90_key, new_percentile,
                                    old_percentile, t);
                              }

                              if let Some(old_v) = tree.get(p99_key.as_bytes())? {
                                  let old_percentile = usize::from_le_bytes((*old_v).try_into().unwrap());
                                  let new_percentile = calculate_percentile(old_percentile, t,
                                                                            standard_dev, 0.99f64);
                                  tree.insert(p99_key.as_bytes(), &new_percentile.to_le_bytes())?;
                                  info!("TIME rewrote {}: {} (old={}, t={})", p99_key, new_percentile,
                                    old_percentile, t);
                              }

                              if let Some(old_v) = tree.get(p99_9_key.as_bytes())? {
                                  let old_percentile = usize::from_le_bytes((*old_v).try_into().unwrap());
                                  let new_percentile = calculate_percentile(old_percentile, t,
                                                                            standard_dev, 0.999f64);
                                  tree.insert(p99_9_key.as_bytes(), &new_percentile.to_le_bytes())?;
                                  info!("TIME rewrote {}: {} (old={}, t={})", p99_9_key, new_percentile,
                                    old_percentile, t);
                              }

                              if let Some(old_v) = tree.get(p99_99_key.as_bytes())? {
                                  let old_percentile = usize::from_le_bytes((*old_v).try_into().unwrap());
                                  let new_percentile = calculate_percentile(old_percentile, t,
                                                                            standard_dev, 0.9999f64);
                                  tree.insert(p99_99_key.as_bytes(), &new_percentile.to_le_bytes())?;
                                  info!("TIME rewrote {}: {} (old={}, t={})", p99_99_key, new_percentile,
                                    old_percentile, t);
                              }

                              if let Some(old_v) = tree.get(p99_999_key.as_bytes())? {
                                  let old_percentile = usize::from_le_bytes((*old_v).try_into().unwrap());
                                  let new_percentile = calculate_percentile(old_percentile, t,
                                                                            standard_dev, 0.99999f64);
                                  tree.insert(p99_999_key.as_bytes(), &new_percentile.to_le_bytes())?;
                                  info!("TIME rewrote {}: {} (old={}, t={})", p99_999_key, new_percentile,
                                    old_percentile, t);
                              }

                              if let Some(old_v) = tree.get(p100_key.as_bytes())? {
                                  let old_percentile = usize::from_le_bytes((*old_v).try_into().unwrap());
                                  // the 100 percentile is the largest value
                                  if t > old_percentile {
                                      tree.insert(p100_key.as_bytes(), &t.to_le_bytes())?;
                                  }
                                  info!("TIME rewrote {}: {} (old={}, t={})", p100_key, t,
                                    old_percentile, t);
                              }
                          }
                      }
                  }
              }
          }
      };

      Ok(())
  }

  pub fn clear(&mut self, now: OffsetDateTime) -> Result<(), sled::Error> {
      info!("will clear old data from the metrics database");
      //self.db.clear();
      //

      let metrics = self.metrics.clone();
      for (key, (meta, kind)) in metrics.iter() {
          info!("will aggregate metrics for key '{}'", key);

          let is_backend = *meta == MetricMeta::ClusterBackend;
          match kind {
              MetricKind::Gauge => {
                  self.aggregate_gauge(key, now, is_backend)?;
              },
              MetricKind::Count => {
                  self.aggregate_count(key, now, is_backend)?;
              },
              MetricKind::Time => {
              }
          }

          let tree = match meta {
              MetricMeta::Cluster => &mut self.cluster_tree,
              MetricMeta::ClusterBackend => &mut self.backend_tree,
          };

          // check if we removed all the points for this metric
          let end = format!("{}\x7F", key);
          if let Some((k, _)) = tree.get_gt(key.as_bytes())? {
              if &k == end.as_bytes() {
                  info!("removing key {} from metrics", key);
                  tree.remove(k)?;
                  self.metrics.remove(key);
              }
          }
      }

      info!("remaining keys:");
      if let (Some(first), Some(second)) = (self.cluster_tree.first()?, self.cluster_tree.last()?) {
        for res in self.cluster_tree.range(first.0..second.0) {
            let (k, v) = res?;
            info!("{} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));

        }
      }
      if let (Some(first), Some(second)) = (self.backend_tree.first()?, self.backend_tree.last()?) {
        for res in self.backend_tree.range(first.0..second.0) {
            let (k, v) = res?;
            info!("{} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));

        }
      }
      info!("db size: {:?}", self.db.size_on_disk());

      Ok(())
  }

}


impl Subscriber for LocalDrain {
  fn receive_metric(&mut self, key: &'static str, cluster_id: Option<&str>, backend_id: Option<&str>, metric: MetricData) {
    if let Some(id) = cluster_id {
      self.receive_cluster_metric(key, id, backend_id, metric);
    } else if !self.data.contains_key(key) {
      self.data.insert(
        String::from(key),
        AggregatedMetric::new(metric)
        );
    } else {
      self.data.get_mut(key).map(|stored_metric| {
        stored_metric.update(key, metric);
      });
    }
  }
}

// implementation of an algorithm from https://mjambon.com/2016-07-23-moving-percentile/
fn calculate_percentile(old_value: usize, measure: usize, standard_deviation: f64, percentile: f64) -> usize {
    // to be adated can be between 0.01 and 0.001
    let r = 0.01f64;
    let delta = standard_deviation * r;

    if measure == old_value {
        old_value
    } else if measure < old_value {
        let new_value = old_value as f64 - delta / percentile;
        new_value as usize
    } else {
        let new_value = old_value as f64 + delta / ( 1f64 - percentile );
        new_value as usize
    }
}
