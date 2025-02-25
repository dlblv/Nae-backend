use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;

use actix::prelude::*;
use crossbeam::channel::{Receiver, Sender};
use json::{array, JsonValue};
use tokio_cron_scheduler::JobScheduler;
use uuid::Uuid;

use crate::services::{Event, Mutation};
use crate::text_search::SearchEngine;
use crate::ws::{engine_io, socket_io, Connect, Disconnect, WsMessage};
use crate::{animo::db::AnimoDB, settings::Settings};
use crate::{storage::Workspaces, ws};
use service::error::Error;
use service::{Service, Services};
use store::wh_storage::WHStorage;
use store::GetWarehouse;

type Socket = Recipient<WsMessage>;

#[derive(Clone)]
pub struct Application {
  pub(crate) settings: Arc<Settings>,
  pub(crate) db: Arc<AnimoDB>,
  pub(crate) job_scheduler: JobScheduler,
  services: Arc<RwLock<HashMap<String, Arc<dyn Service>>>>,

  pub wss: Workspaces,
  pub(crate) warehouse: WHStorage,

  // background dispatcher
  stop: Arc<AtomicBool>,
  pub(crate) events: Sender<Event>,
  pub(crate) sender: Sender<Mutation>,

  pub search: Arc<RwLock<SearchEngine>>,
}

impl GetWarehouse for Application {
  fn warehouse(&self) -> WHStorage {
    self.warehouse.clone()
  }
}

impl Application {
  pub async fn new(
    settings: Arc<Settings>,
    db: Arc<AnimoDB>,
    wss: Workspaces,
  ) -> Result<(Self, Receiver<Event>), Error> {
    let services: Arc<RwLock<HashMap<String, Arc<dyn Service>>>> =
      Arc::new(RwLock::new(HashMap::new()));

    let job_scheduler = tokio_cron_scheduler::JobScheduler::new()
      .await
      .map_err(|e| Error::GeneralError(e.to_string()))?;

    let (events_sender, events_receiver) = crossbeam::channel::bounded(1);
    let (sender, receiver) = crossbeam::channel::bounded(1);
    let stop = Arc::new(AtomicBool::new(false));

    let app = Application {
      settings: settings.clone(),
      db,
      job_scheduler,
      services,
      wss,
      warehouse: WHStorage::open(&settings.database.inventory)
        .map_err(|e| Error::GeneralError(e.message()))?,
      // channels: Arc::new(HashMap::new()),
      stop: stop.clone(),
      events: events_sender,
      sender,
      search: Arc::new(RwLock::new(SearchEngine::new())),
    };

    thread::spawn({
      let should_stop = stop.clone();
      let r = receiver.clone();
      let a = app.clone();
      move || {
        while !should_stop.load(Ordering::SeqCst) {
          match r.recv() {
            Ok(mutation) => {
              println!("mutation {:?}", mutation);
              match a.handle(mutation) {
                Ok(_) => {},
                Err(e) => {
                  println!("error at dispatcher thread {}", e);
                },
              }
            },
            Err(e) => {
              println!("exist dispatcher thread because of {}", e);
              break;
            },
          }
        }
      }
    });

    Ok((app, events_receiver))
  }

  pub(crate) fn handle(&self, mutation: Mutation) -> crate::services::Result {
    match mutation {
      Mutation::Create(ctx, name, data, params) => {
        self.service(&name).create(ctx, data, params).map(|data| {
          self.emit(Event::Created(name, data.clone()));
          data
        })
      },
      Mutation::Update(ctx, name, id, data, params) => {
        self.service(&name).update(ctx, id, data, params).map(|data| {
          self.emit(Event::Updated(name, data.clone()));
          data
        })
      },
      Mutation::Patch(ctx, name, id, data, params) => {
        self.service(&name).patch(ctx, id, data, params).map(|data| {
          self.emit(Event::Patched(name, data.clone()));
          data
        })
      },
      Mutation::Remove(ctx, name, id, params) => {
        self.service(&name).remove(ctx, id, params).map(|data| {
          self.emit(Event::Removed(name, data.clone()));
          data
        })
      },
    }
  }

  fn emit(&self, event: Event) {
    println!("event {:?}", event);

    // workaround to close authentication, users and actions service
    let service_name = match &event {
      Event::Created(name, _) => name,
      Event::Updated(name, _) => name,
      Event::Patched(name, _) => name,
      Event::Removed(name, _) => name,
    };
    if service_name == "authentication" || service_name == "users" {
      // TODO || service_name == "actions" {
      return;
    }

    self.events.send(event).unwrap()
  }

  pub(crate) fn close(self) {
    // TODO self.db.close();
    self.stop.store(true, Ordering::SeqCst);
  }
}

impl Services for Application {
  fn register(&mut self, service: Arc<dyn Service>) {
    let path = service.path().to_string();

    let mut services = self.services.write().unwrap();
    if let Some(current) = services.insert(path, service) {
      panic!("service for path {:?} already registered", current.path());
    }
  }

  fn service<S: AsRef<str> + ToString>(&self, name: S) -> Arc<dyn Service> {
    let services = self.services.read().unwrap();
    if let Some(service) = services.get(name.as_ref()) {
      service.clone()
    } else {
      Arc::new(service::NoService(name.to_string()))
    }
  }
}

#[derive(Clone)]
pub struct Commutator {
  app: Application,
  sessions: Arc<RwLock<HashMap<Uuid, Socket>>>,
  stop: Arc<AtomicBool>,
}

impl Commutator {
  pub(crate) fn new(app: Application, events: Receiver<Event>) -> Commutator {
    let stop = Arc::new(AtomicBool::new(false));

    let com = Commutator {
      app,
      sessions: Arc::new(RwLock::new(HashMap::new())),
      // rooms: HashMap::new(),
      stop: stop.clone(),
    };

    thread::spawn({
      let should_stop = stop.clone();
      let c = com.clone();
      move || {
        while !should_stop.load(Ordering::SeqCst) {
          match events.recv() {
            Ok(event) => {
              println!("sending to all: {:?}", event);
              let (name, data) = match event {
                Event::Created(name, data) => (format!("{name} created"), data),
                Event::Updated(name, data) => (format!("{name} updated"), data),
                Event::Patched(name, data) => (format!("{name} patched"), data),
                Event::Removed(name, data) => (format!("{name} removed"), data),
              };
              let data = array![JsonValue::String(name.clone()), data];
              c.event_to_all(data.dump());
            },
            Err(e) => {
              println!("exist dispatcher thread because of {}", e);
              break;
            },
          }
        }
      }
    });

    com
  }

  fn open(&self, sid: &Uuid) {
    let sessions = self.sessions.read().unwrap();
    if let Some(socket) = sessions.get(sid) {
      socket.do_send(WsMessage::open(sid));

      // version 4: "0{\"sid\":\"...\"}"
      socket.do_send(WsMessage {
        data: format!("{{\"sid\":\"{}\"}}", sid.to_string()),
        engine_code: engine_io::MESSAGE.to_string(),
        socket_code: Some(socket_io::CONNECT.to_string()),
      });
      // version 3: "0"
      // socket.do_send(WsMessage {
      //   data: "".to_string(),
      //   engine_code: engine_io::MESSAGE.to_string(),
      //   socket_code: Some(socket_io::CONNECT.to_string()),
      // });
    } else {
      println!("attempting to send message but couldn't find user id.");
    }
  }

  fn event_to_all(&self, response: String) {
    let sessions = self.sessions.read().unwrap();
    for socket in sessions.values() {
      socket.do_send(WsMessage::event(response.clone()));
    }
  }

  fn event(&self, response: String, id_to: &Uuid) {
    let sessions = self.sessions.read().unwrap();
    if let Some(socket) = sessions.get(id_to) {
      socket.do_send(WsMessage::event(response));
    } else {
      println!("attempting to send message but couldn't find user id.");
    }
  }

  fn ack(&self, event_id: String, response: String, id_to: &Uuid) {
    let sessions = self.sessions.read().unwrap();
    if let Some(socket) = sessions.get(id_to) {
      socket.do_send(WsMessage::ack(event_id, response));
    } else {
      println!("attempting to send message but couldn't find user id.");
    }
  }
}

impl Actor for Commutator {
  type Context = Context<Self>;
}

fn data_params(mut data: JsonValue) -> Result<(JsonValue, JsonValue), Error> {
  Ok((data.array_remove(0), data.array_remove(0)))
}

fn id_params(mut data: JsonValue) -> Result<(String, JsonValue), Error> {
  if let Some(id) = data.array_remove(0).as_str() {
    Ok((id.to_string(), data.array_remove(0)))
    // match ID::from_base64(id.as_bytes()) {
    //     Ok(id) => Ok((id, data.array_remove(0))),
    //     Err(_) => Err(Error::GeneralError(format!("incorrect id {}", id))),
    // }
  } else {
    Err(Error::GeneralError("not found id".to_string()))
  }
}

fn id_data_params(mut data: JsonValue) -> Result<(String, JsonValue, JsonValue), Error> {
  if let Some(id) = data.array_remove(0).as_str() {
    Ok((id.to_string(), data.array_remove(0), data.array_remove(0)))
    // match ID::from_base64(id.as_bytes()) {
    //     Ok(id) => Ok((id, data.array_remove(0), data.array_remove(0))),
    //     Err(_) => Err(Error::GeneralError(format!("incorrect id {}", id))),
    // }
  } else {
    Err(Error::GeneralError("not found id".to_string()))
  }
}

impl Handler<ws::Event> for Commutator {
  type Result = ();

  fn handle(&mut self, msg: ws::Event, _ctx: &mut Self::Context) -> Self::Result {
    let service = self.app.service(msg.path.as_str());
    let response = match msg.command.as_str() {
      "find" => service.find(msg.ctx, msg.data),
      "get" => id_params(msg.data).and_then(|(id, params)| service.get(msg.ctx, id, params)),
      "create" => data_params(msg.data).and_then(|(data, params)| {
        self.app.handle(Mutation::Create(msg.ctx, msg.path, data, params))
      }),
      "update" => id_data_params(msg.data).and_then(|(id, data, params)| {
        self.app.handle(Mutation::Update(msg.ctx, msg.path, id, data, params))
      }),
      "patch" => id_data_params(msg.data).and_then(|(id, data, params)| {
        self.app.handle(Mutation::Patch(msg.ctx, msg.path, id, data, params))
      }),
      "remove" => id_params(msg.data)
        .and_then(|(id, params)| self.app.handle(Mutation::Remove(msg.ctx, msg.path, id, params))),
      _ => Err(Error::GeneralError(format!(
        "service '{}' do not have command '{}'",
        msg.path, msg.command
      ))),
    };

    let response = match response {
      Ok(data) => json::array![JsonValue::Null, data],
      Err(err) => json::array![err.to_json()],
    };
    self.ack(msg.event_id, response.dump(), &msg.sid)
  }
}

impl Handler<Connect> for Commutator {
  type Result = ();

  fn handle(&mut self, msg: Connect, _ctx: &mut Self::Context) -> Self::Result {
    {
      let mut sessions = self.sessions.write().unwrap();
      sessions.insert(msg.sid, msg.socket);
    }

    self.open(&msg.sid);
  }
}

impl Handler<Disconnect> for Commutator {
  type Result = ();

  fn handle(&mut self, msg: Disconnect, _ctx: &mut Self::Context) -> Self::Result {
    let mut sessions = self.sessions.write().unwrap();
    if sessions.remove(&msg.sid).is_some() {
      // TODO remove from channels
    }
  }
}
