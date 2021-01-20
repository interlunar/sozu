use std::str;
use std::time::Instant;
use std::convert::TryInto;
use std::collections::BTreeMap;
use hdrhistogram::Histogram;
use sozu_command::proxy::{FilteredData,MetricsData,Percentiles,AppMetricsData};

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

#[derive(Clone,Debug)]
enum MetricKind {
  Gauge,
  Count,
  Time,
}

#[derive(Clone,Debug)]
enum MetricMeta {
    Cluster,
    ClusterBackend,
}

#[derive(Debug)]
pub struct LocalDrain {
  pub prefix:          String,
  pub created:         Instant,
  pub db:              sled::Db,
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

    LocalDrain {
      prefix,
      created:     Instant::now(),
      db,
      metrics:     BTreeMap::new(),
      data:        BTreeMap::new(),
      use_tagged_metrics: false,
      origin:      String::from("x"),
    }
  }

  pub fn dump_metrics_data(&mut self) -> MetricsData {
    MetricsData {
      proxy:        self.dump_process_data(),
      clusters: self.dump_cluster_data(),
    }
  }

  pub fn dump_process_data(&mut self) -> BTreeMap<String, FilteredData> {
    let data: BTreeMap<String, FilteredData> = self.data.iter().map(|(ref key, ref value)| {
      (key.to_string(), aggregated_to_filtered(value))
    }).collect();

    data
  }

  pub fn dump_cluster_data(&mut self) -> BTreeMap<String,AppMetricsData> {
      let mut apps = BTreeMap::new();

      for (key, (meta, kind)) in self.metrics.iter() {
          let end = format!("{}\x7F", key);
          for res in self.db.range(key.as_bytes()..end.as_bytes()) {
              let (k, v) = res.unwrap();
              match meta {
                  MetricMeta::Cluster => {
                      let mut it = k.split(|c: &u8| *c == b'.');
                      let key = std::str::from_utf8(it.next().unwrap()).unwrap();
                      let app_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                      let timestamp:i64 = std::str::from_utf8(it.next().unwrap()).unwrap().parse().unwrap();

                      info!("looking at key = {}, id = {}, ts = {}",
                            key, app_id, timestamp);

                      let metrics_data = apps.entry(app_id.to_string()).or_insert_with(AppMetricsData::new);
                      match kind {
                          MetricKind::Gauge => {
                              if metrics_data.data.contains_key(key) {
                                  let v2 = metrics_data.data.get(key).unwrap().clone();
                              } else {
                                  metrics_data.data.insert(key.to_string(), FilteredData::Gauge(usize::from_le_bytes((*v).try_into().unwrap())));
                              }
                          },
                          MetricKind::Count => {
                              if metrics_data.data.contains_key(key) {
                                  let v2 = metrics_data.data.get(key).unwrap().clone();
                              } else {
                                  metrics_data.data.insert(key.to_string(), FilteredData::Count(i64::from_le_bytes((*v).try_into().unwrap())));
                              }
                          },
                          MetricKind::Time => {
                              //unimplemented for now
                          }
                      }
                  },
                  MetricMeta::ClusterBackend => {
                      let mut it = k.split(|c: &u8| *c == b'\t');
                      let key = std::str::from_utf8(it.next().unwrap()).unwrap();
                      let app_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                      let backend_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                      let timestamp:i64 = std::str::from_utf8(it.next().unwrap()).unwrap().parse().unwrap();

                      info!("looking at key = {}, ap id = {}, bid: {}, ts = {}",
                            key, app_id, backend_id, timestamp);

                      let app_metrics_data = apps.entry(app_id.to_string()).or_insert_with(AppMetricsData::new);
                      let backend_metrics_data = app_metrics_data.backends.entry(backend_id.to_string()).or_insert_with(BTreeMap::new);
                      match kind {
                          MetricKind::Gauge => {
                              if backend_metrics_data.contains_key(key) {
                                  let v2 = backend_metrics_data.get(key).unwrap().clone();
                              } else {
                                  backend_metrics_data.insert(key.to_string(), FilteredData::Gauge(usize::from_le_bytes((*v).try_into().unwrap())));
                              }
                          },
                          MetricKind::Count => {
                              if backend_metrics_data.contains_key(key) {
                                  let v2 = backend_metrics_data.get(key).unwrap().clone();
                              } else {
                                  backend_metrics_data.insert(key.to_string(), FilteredData::Count(i64::from_le_bytes((*v).try_into().unwrap())));
                              }
                          },
                          MetricKind::Time => {
                              //unimplemented for now
                          }
                      }
                  }
              }
          }
      }


      // still clear the DB for now
      self.db.clear();

      apps
  }

  pub fn clear(&mut self) {
      self.db.clear();
  }

  fn receive_cluster_metric(&mut self, key: &'static str, id: &str, backend_id: Option<&str>, metric: MetricData) {
      info!("metric: {} {} {:?} {:?}", key, id, backend_id, metric);

      if !self.metrics.contains_key(key) {
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

          self.metrics.insert(key.to_string(), (meta, kind));
          //let start = format!("{}\0", key);
          let end = format!("{}\x7F", key);
          //self.db.insert(start.as_bytes(), &0u64.to_le_bytes()).unwrap();
          self.db.insert(end.as_bytes(), &0u64.to_le_bytes()).unwrap();
      }

      let now = time::OffsetDateTime::now();
      // reduce to mucrosecond precision
      let ts = now.timestamp_nanos() / 1000;

      let metric_type = match metric {
          MetricData::Gauge(_) => 'g',
          MetricData::GaugeAdd(_) => 'a',
          MetricData::Count(_) => 'c',
          MetricData::Time(_) => 't',
      };

      let db_key = if let Some(bid) = backend_id {
          format!("{}\t{}\t{}\t{}", key, id, bid, ts)
      } else {
          format!("{}\t{}\t{}", key, id, ts)
      };

      match metric {
          MetricData::Gauge(i) => {
              self.db.insert(db_key.as_bytes(), &i.to_le_bytes()).unwrap();
          },
          MetricData::GaugeAdd(i) => {
              self.db.insert(db_key.as_bytes(), &i.to_le_bytes()).unwrap();
          },
          MetricData::Count(i) => {
              self.db.insert(db_key.as_bytes(), &i.to_le_bytes()).unwrap();
          },
          MetricData::Time(i) => {
              self.db.insert(db_key.as_bytes(), &i.to_le_bytes()).unwrap();
          },
      }

      /*
      if let (Some(first), Some(second)) = (self.db.first().unwrap(), self.db.last().unwrap()) {
        for res in self.db.range(first.0..second.0) {
            let (k, v) = res.unwrap();
            info!("{} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));

        }
      }
      info!("metrics: {:?}", self.metrics);
      info!("db size (at {}): {:?}", self.data_dir.path().to_str().unwrap(), self.db.size_on_disk());
      */
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

