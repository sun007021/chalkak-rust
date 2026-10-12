//! 원본 `PostCommandService`: 게시물과 이미지 업로드의 상태를 바꾸는 흐름.
//!
//! 업로드 권한 발급, 게시물 생성, 이미지 처리 결과 반영이 모두 같은 업로드 행을 잠그고 겨루므로 한자리에 둔다.
//! 잠금 순서(업로드 -> 게시물)는 원본과 같게 지킨다. 서비스 메서드 하나가 트랜잭션 하나다.

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::error::{AppError, ErrorCode};
use chalkak_core::log::{Logger, fields};
use chalkak_db::Pool;
use jiff::Timestamp;
use postgres::GenericClient;
use serde_json::{Map, Value};
use uuid::Uuid;

use super::domain::{ModerationStatus, Post, PostImageUpload, PostImageUploadStatus};
use super::listener::PostModerationPendingListener;
use super::policy::PostProcessingPolicy;
use super::repository::PostRepository;
use super::storage::{PostImageStorage, PostProcessingImageUpload, StorageError};
use super::support::{in_transaction, require_active_user};
use super::upload_repository::PostImageUploadRepository;
use crate::photo::domain::Photo;
use crate::photo::repository::PhotoRepository;
use crate::topic::domain::{Topic, TopicPhase};
use crate::topic::repository::TopicRepository;
use crate::web::ApiError;

const MODERATION_LOGGER: &str = "chalkak.moderation";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostImageUploadResult {
    pub upload_id: Uuid,
    pub upload_url: String,
    pub expires_in_seconds: u64,
    pub content_type: String,
    pub max_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PostCreationResult {
    pub post_id: Uuid,
    pub moderation_status: ModerationStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostUpdateResult {
    pub post_id: Uuid,
    pub title: Option<String>,
}

fn not_found(message: &str) -> ApiError {
    ApiError::App(AppError::not_found(ErrorCode::BUSINESS_ERROR, message))
}

fn business(message: &str) -> ApiError {
    ApiError::App(AppError::business(ErrorCode::BUSINESS_ERROR, message))
}

pub(crate) fn storage_failure(error: StorageError) -> ApiError {
    ApiError::Internal(error.to_string())
}

#[derive(Clone)]
pub struct PostCommandService {
    pool: Pool,
    clock: Arc<dyn Clock>,
    logger: Arc<Logger>,
    storage: PostImageStorage,
    policy: PostProcessingPolicy,
    listener: Arc<dyn PostModerationPendingListener>,
    posts: PostRepository,
    uploads: PostImageUploadRepository,
    photos: PhotoRepository,
    topics: TopicRepository,
}

impl PostCommandService {
    pub fn new(
        pool: Pool,
        clock: Arc<dyn Clock>,
        logger: Arc<Logger>,
        storage: PostImageStorage,
        policy: PostProcessingPolicy,
        listener: Arc<dyn PostModerationPendingListener>,
    ) -> Self {
        PostCommandService {
            pool,
            clock,
            logger,
            storage,
            policy,
            listener,
            posts: PostRepository::default(),
            uploads: PostImageUploadRepository,
            photos: PhotoRepository,
            topics: TopicRepository,
        }
    }

    /// `createPostImageUpload`
    pub fn create_post_image_upload(
        &self,
        user_id: Uuid,
    ) -> Result<PostImageUploadResult, ApiError> {
        in_transaction(&self.pool, |tx| {
            require_active_user(tx, user_id)?;
            let now = self.clock.now();
            let mut upload = PostImageUpload::create(user_id, now);
            self.uploads.insert(tx, &mut upload, now)?;
            let upload_id = upload.id.expect("저장한 업로드에는 식별자가 있다");
            let presigned = self.storage.issue(upload_id).map_err(storage_failure)?;
            Ok(PostImageUploadResult {
                upload_id,
                upload_url: presigned.upload_url,
                expires_in_seconds: presigned.expires_in_seconds,
                content_type: presigned.content_type,
                max_bytes: presigned.max_bytes,
            })
        })
    }

    /// `issuePostImageProcessingUpload`: DB를 건드리지 않는다.
    pub fn issue_post_image_processing_upload(
        &self,
        upload_id: Uuid,
    ) -> Result<PostProcessingImageUpload, ApiError> {
        self.storage
            .issue_processing_upload(upload_id)
            .map_err(storage_failure)
    }

    /// `createPost`
    pub fn create_post(
        &self,
        user_id: Uuid,
        topic_id: Uuid,
        photo_upload_id: Uuid,
        title: Option<&str>,
    ) -> Result<PostCreationResult, ApiError> {
        let mut pending = Vec::new();
        let result = in_transaction(&self.pool, |tx| {
            self.create_post_in(tx, user_id, topic_id, photo_upload_id, title, &mut pending)
        })?;
        // 커밋이 끝난 뒤에 알린다(`@TransactionalEventListener(AFTER_COMMIT)`).
        self.notify_pending(&pending);
        Ok(result)
    }

    fn create_post_in(
        &self,
        tx: &mut impl GenericClient,
        user_id: Uuid,
        topic_id: Uuid,
        photo_upload_id: Uuid,
        title: Option<&str>,
        pending: &mut Vec<(Uuid, Timestamp)>,
    ) -> Result<PostCreationResult, ApiError> {
        require_active_user(tx, user_id)?;
        let topic = self
            .topics
            .find_active_by_id(tx, topic_id)?
            .ok_or_else(|| not_found("게시물을 작성할 주제를 찾을 수 없습니다."))?;
        self.validate_topic_open(&topic)?;

        // S3 확인은 왕복이 길 수 있다. 업로드 행에 잠금을 잡은 채 기다리면 같은 업로드의 처리 콜백이 그동안
        // 락 대기에 묶이므로 락 밖에서 먼저 확인한다.
        let staging_image_exists = self.needs_staging_image_check(tx, user_id, photo_upload_id)?
            && self
                .storage
                .exists_uploaded_image(photo_upload_id)
                .map_err(storage_failure)?;

        let mut upload = self.claimable_upload(tx, user_id, photo_upload_id)?;
        upload.claim(self.clock.now())?;

        // 이미지 처리 콜백도 업로드 다음 게시물 순서로 잠근다. 모든 경로의 순서를 통일해야 처리 시간 초과
        // 게시물 재작성과 콜백이 겹쳐도 순환 대기가 생기지 않는다.
        self.validate_post_not_created(tx, user_id, topic_id, self.clock.now())?;

        // 확인한 뒤에 처리가 끝나 staging이 지워졌을 수 있으므로 최종 판정은 락 안에서 읽은 상태로 한다.
        if !upload.is_processed() && !staging_image_exists {
            return Err(not_found("업로드한 사진을 찾을 수 없습니다."));
        }

        let original_key = self.storage.to_original_storage_key(photo_upload_id);
        if self
            .photos
            .exists_by_original_storage_key(tx, &original_key)?
        {
            return Err(business("이미 사용된 사진입니다."));
        }
        let photo = Photo::create(Some(&original_key))?;
        let mut post = Post::create(user_id, topic_id, photo, Some(photo_upload_id), title)?;
        if upload.is_processed() {
            post.photo.complete_processing(
                self.storage.to_thumbnail_storage_key(photo_upload_id),
                upload.image_metadata.clone(),
            );
            post.request_moderation()?;
        }

        // 업로드의 사용 표시는 게시물 INSERT와 같은 트랜잭션에서 반영된다.
        self.uploads.update(tx, &upload, self.clock.now())?;
        self.posts.save(tx, &mut post, self.clock.now())?;
        let post_id = post.id.expect("저장한 게시물에는 식별자가 있다");
        if post.moderation_status == ModerationStatus::Pending {
            pending.push(self.record_moderation_pending(post_id));
        }
        Ok(PostCreationResult {
            post_id,
            moderation_status: post.moderation_status,
        })
    }

    /// `deletePost`
    pub fn delete_post(&self, author_id: Uuid, post_id: Uuid) -> Result<(), ApiError> {
        in_transaction(&self.pool, |tx| {
            let mut post = self
                .posts
                .find_by_id_for_update(tx, post_id)?
                .ok_or_else(|| not_found("게시물을 찾을 수 없습니다."))?;
            let was_deleted = post.deleted_at.is_some();
            post.delete_by_author(author_id, Some(self.clock.now()))?;
            if !was_deleted {
                self.posts.update_deleted(tx, &post, self.clock.now())?;
            }
            // 좋아요는 삭제 여부와 상관없이 지운다(멱등).
            tx.execute("DELETE FROM post_likes WHERE post_id = $1", &[&post_id])
                .map_err(chalkak_db::DbError::from)?;
            Ok(())
        })
    }

    /// `updatePost`
    pub fn update_post(
        &self,
        author_id: Uuid,
        post_id: Uuid,
        title: Option<&str>,
    ) -> Result<PostUpdateResult, ApiError> {
        in_transaction(&self.pool, |tx| {
            require_active_user(tx, author_id)?;
            let mut post = self
                .posts
                .find_active_by_id_for_update(tx, post_id)?
                .ok_or_else(|| not_found("게시물을 찾을 수 없습니다."))?;
            let now = self.clock.now();
            // 수정 가능 기간은 게시물이 속한 주제의 참여 기간으로 정한다.
            let phase = match self.posts.find_topic_period(tx, post.topic_id)? {
                Some(period) => period.phase_at(now),
                None => TopicPhase::Closed,
            };
            let before = post.title.clone();
            post.update_title(author_id, title, phase)?;
            if post.title != before {
                self.posts.update_title(tx, &post, now)?;
            }
            Ok(PostUpdateResult {
                post_id,
                title: post.title_value().map(str::to_owned),
            })
        })
    }

    /// `completePostImageProcessing`: 게시물이 아직 없으면 업로드 상태만 바꾸고 끝낸다. 나중에 도착하는 게시물 생성
    /// 요청이 READY를 보고 사진 처리를 반영한 뒤 관리자 검수 대기 상태로 만든다.
    pub fn complete_post_image_processing(
        &self,
        upload_id: Uuid,
        image_metadata: Map<String, Value>,
    ) -> Result<(), ApiError> {
        let mut pending = Vec::new();
        in_transaction(&self.pool, |tx| {
            let Some(mut upload) = self.uploads.find_by_id_for_update(tx, upload_id)? else {
                return Ok(());
            };
            if upload.complete_processing(image_metadata) {
                self.uploads.update(tx, &upload, self.clock.now())?;
            }
            if !upload.is_processed() {
                return Ok(());
            }
            if let Some(mut post) = self
                .posts
                .find_validating_by_upload_id_for_update(tx, upload_id)?
            {
                post.photo.complete_processing(
                    self.storage.to_thumbnail_storage_key(upload_id),
                    upload.image_metadata.clone(),
                );
                post.request_moderation()?;
                let now = self.clock.now();
                self.posts.update_photo_processing(tx, &post, now)?;
                self.posts.update_moderation(tx, &post, now)?;
                let post_id = post.id.expect("읽은 게시물에는 식별자가 있다");
                pending.push(self.record_moderation_pending(post_id));
            }
            Ok(())
        })?;
        self.notify_pending(&pending);
        Ok(())
    }

    /// `failPostImageProcessing`
    pub fn fail_post_image_processing(
        &self,
        upload_id: Uuid,
        rejection_reason: Option<&str>,
    ) -> Result<(), ApiError> {
        in_transaction(&self.pool, |tx| {
            let Some(mut upload) = self.uploads.find_by_id_for_update(tx, upload_id)? else {
                return Ok(());
            };
            if upload.fail_processing(rejection_reason) {
                self.uploads.update(tx, &upload, self.clock.now())?;
            }
            if !upload.is_rejected() {
                return Ok(());
            }
            if let Some(mut post) = self
                .posts
                .find_validating_by_upload_id_for_update(tx, upload_id)?
            {
                post.fail_image_processing()?;
                self.posts.update_moderation(tx, &post, self.clock.now())?;
            }
            Ok(())
        })
    }

    /// 검수 대기 로그를 남기고 (게시물, 발생 시각)을 돌려준다. 호출 측이 커밋 뒤에 구독자를 부른다.
    fn record_moderation_pending(&self, post_id: Uuid) -> (Uuid, Timestamp) {
        self.logger
            .info(MODERATION_LOGGER)
            .field(fields::TYPE, fields::TYPE_MODERATION)
            .field(fields::EVENT, "pending")
            .field(fields::POST_ID, post_id.to_string())
            .log("검수 대기");
        (post_id, self.clock.now())
    }

    fn notify_pending(&self, pending: &[(Uuid, Timestamp)]) {
        for (post_id, occurred_at) in pending {
            self.listener.on_pending(*post_id, *occurred_at);
        }
    }

    fn validate_topic_open(&self, topic: &Topic) -> Result<(), ApiError> {
        if topic.phase_at(self.clock.now()) != TopicPhase::Open {
            return Err(business("현재 게시물을 작성할 수 없는 주제입니다."));
        }
        Ok(())
    }

    /// 이미지 처리 콜백이 유실되거나 영구 거부되면 게시물이 이미지 처리 대기 상태로 남는다. 작성자에게는 보이지도
    /// 않으면서 같은 주제 재작성만 막으므로, 처리 대기 시간을 넘긴 게시물은 여기서 거절 처리하고 길을 터 준다.
    fn validate_post_not_created(
        &self,
        tx: &mut impl GenericClient,
        user_id: Uuid,
        topic_id: Uuid,
        now: Timestamp,
    ) -> Result<(), ApiError> {
        let Some(mut post) = self
            .posts
            .find_active_by_author_and_topic(tx, user_id, topic_id, true)?
        else {
            return Ok(());
        };
        let timed_out = post.is_validating()
            && post
                .created_at
                .is_some_and(|created| self.policy.is_processing_timed_out(created, now));
        if timed_out {
            post.fail_image_processing()?;
            // 부분 유니크 인덱스가 REJECTED만 제외하므로, 새 게시물 INSERT보다 이 거절이 먼저 반영돼야 한다.
            self.posts.update_moderation(tx, &post, self.clock.now())?;
            return Ok(());
        }
        Err(business("이미 해당 주제에 게시물을 작성했습니다."))
    }

    /// 다른 회원의 uploadId는 권한 없음이 아니라 없는 것으로 답한다. 존재 여부를 알려주지 않기 위해서다.
    fn claimable_upload(
        &self,
        tx: &mut impl GenericClient,
        user_id: Uuid,
        upload_id: Uuid,
    ) -> Result<PostImageUpload, ApiError> {
        self.uploads
            .find_by_id_for_update(tx, upload_id)?
            .filter(|upload| upload.is_owned_by(user_id))
            .ok_or_else(|| not_found("업로드한 사진을 찾을 수 없습니다."))
    }

    /// 이미 처리가 끝났으면 staging 객체는 지워진 뒤라 확인할 필요가 없고, 없거나 남의 업로드면 어차피
    /// [`Self::claimable_upload`]가 없는 것으로 답하므로 S3까지 갈 이유가 없다.
    fn needs_staging_image_check(
        &self,
        tx: &mut impl GenericClient,
        user_id: Uuid,
        upload_id: Uuid,
    ) -> Result<bool, ApiError> {
        Ok(self
            .uploads
            .find_status_by_id_and_user_id(tx, upload_id, user_id)?
            .is_some_and(|status| status != PostImageUploadStatus::Ready))
    }
}
