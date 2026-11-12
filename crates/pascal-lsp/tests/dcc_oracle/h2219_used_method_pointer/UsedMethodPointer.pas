unit UsedMethodPointer;

interface

type
  TNotify = procedure of object;
  TFoo = class
  private
    FUnused: Integer;
    FOnEvent: TNotify;
    FOnOtherEvent: TNotify;
    procedure Handler;
    procedure OtherHandler;
  public
    procedure Bind;
  end;

implementation

procedure TFoo.Handler;
begin
end;

procedure TFoo.OtherHandler;
begin
end;

procedure TFoo.Bind;
begin
  FOnEvent := Handler;
  TMethod(FOnOtherEvent).Code := @TFoo.OtherHandler;
  TMethod(FOnOtherEvent).Data := Self;
end;

end.
